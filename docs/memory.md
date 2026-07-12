# Memory model

> **Status.** This doc is a working set of defaults — push back if a proposal conflicts. The non-negotiables in [conformance.md](conformance.md) are settled; the memory model below is a design *direction* we hold to until something better comes along. Flag conflicts; we'll resolve explicitly.

Coddl avoids both tracing garbage collection and Rust-style borrow tracking. It does so by being a value-semantics language with no user-facing references — neither piece of machinery is needed because the situations they exist to handle are unrepresentable. The implementation strategy is **atomic reference counting + copy-on-write + persistent data structures + per-scope arenas** — Swift's ARC + Clojure's persistent collections + Erlang's per-process heaps, three production-proven techniques that compose without conflicting.

## Why no tracing GC

Tracing GC exists primarily to reclaim cycles in the reference graph. Coddl's data graph is cycle-free by construction:

- Tuples, relations, and scalars are values (RM Pre 8 observational equality — see [conformance.md](conformance.md)).
- OO Pro 2 forbids pointer attributes — relations can't reference each other by identity.
- Immutable values can only reference things that existed at the time of their construction, so the reference graph is a DAG.
- Closures capture by value (see "Discipline" below), so no closure can introduce a back-edge.

A DAG of refcounted values frees correctly in topological order when a root refcount hits zero. There are no cycles to collect; refcounting is sufficient.

## Why no borrow checker

Borrow checking prevents two co-existing references where one mutates — use-after-free, iterator invalidation, data races. Coddl makes those situations unrepresentable:

- Values are passed by value; the runtime decides whether that's an `Rc` bump or an actual copy.
- No `&` / `&mut` / `Box` / `Rc` in the surface language.
- Mutable locals (`var x := …; x := …;`) are stack slots that don't escape; no shared references to them exist.
- "Mutation" of a heap value (`var xs := …; xs := xs ++ [item];`) produces a new value. If the original had a single owner, copy-on-write turns it into in-place mutation; otherwise structural sharing makes the new value cheap.

The borrow checker's job — preventing aliased mutation — is done by the *type system* (no way to obtain a mutable alias), not by lifetime tracking.

## Surface vs implementation: two layers

Value semantics is a property of the *surface language*, not the compiled output. The user never sees a pointer, never sees an allocation, never sees a lifetime; the compiler and runtime emit pointers, stack frames, heap allocations, and refcount operations everywhere they help performance. Coddl is aiming for a **production-grade implementation** — the playbook is identical to what Swift, OCaml, and ML-family compilers already do at scale.

| Layer | What it sees |
|---|---|
| Source / AST / typed-AST | Values. No `&`, no allocation, no lifetime. |
| ProcIR (SSA) | SSA values with concrete representations — `Tuple { a: Integer }` is a register-resident scalar; `Text` is `*RcBox<TextRepr>`. See [procir.md](procir.md). |
| LLVM IR / machine code | Explicit `alloca`, `getelementptr`, `load`, `store`, refcount intrinsics, native ints, native pointers. See [codegen.md](codegen.md). |

What the compiler does with the surface guarantee, behind the scenes (none of this is user-visible, none of it requires user annotation):

- **Escape analysis** stack-allocates values that don't outlive their function — no heap touch, no refcount ops.
- **Move optimization** transfers ownership when the caller's copy is dead (refcount `1 → 1`, not `1 → 2 → 1`). A small Coddl-aware pass plus LLVM's optimizer take care of this.
- **Refcount elision** removes `incref`/`decref` pairs that cancel within one function.
- **Scalar replacement of aggregates (SROA)** breaks up tuples never observed as a whole into register-resident scalars.
- **Specialisation** monomorphizes relation-polymorphic operators per heading at compile time; the runtime sees concrete types and concrete layouts. See [runtime.md](runtime.md) "Reaching the engines" for the fallback when specialization isn't possible.
- **Small-value inlining** keeps small `Integer`s, `Character`s, `Boolean`s, `Byte`s unboxed, and likely small `Text`/`Binary` too — small-string-optimization-style.

Stack vs heap vs arena at runtime is decided by the compiler from data-flow analysis, not by user annotation:

| On the stack | On the heap (refcounted) | In a per-scope arena |
|---|---|---|
| Primitives | `Text`, `Binary` beyond an inline-storage threshold | Per-query / per-transaction scratch |
| Non-escaping tuples (post escape analysis) | `Sequence T` buffers | Materialised intermediate relations |
| `let mut` locals | `Relation H` plan handles + materialized rows | Lex / parse output for one source file |
| Short-lived refcount cells | Closure captures that outlive their frame | The CST for one buffer |

The two layers exist deliberately: the user reasons about *values*; the compiler reasons about *representations*. That separation is what lets Coddl have a clean value-semantics language *and* native-speed compiled output — neither paying GC tax nor demanding lifetime annotations from the user.

## Implementation strategy

| Layer | Mechanism |
|---|---|
| Primitives (`Integer`, `Rational`, `Approximate`, `Boolean`, `Character`, `Byte`) | Unboxed value types on the stack. `Integer` is a bignum, so it's boxed under the hood with small-integer optimization. |
| Boxed values (`Text`, `Binary`, `Tuple H`, `Relation H`, `Sequence T`) | Heap-allocated, **atomic reference counting**, freed at refcount = 0. |
| Compound updates (sequence concat, tuple-field update, relation insert) | Structural sharing + copy-on-write. If refcount = 1, mutate the buffer in place; otherwise allocate a new one referencing the old's tail. |
| Per-query / per-transaction scratch | Bump arena, freed wholesale at scope end. |
| Mutable locals | Stack slots holding a value (boxed or unboxed). Rebinding decrements old refcount, increments new. |
| Cross-thread sharing | Atomic refcount; Coddl values are `Send + Sync` for free because they're immutable. |

**What we lose vs. Rust**: zero-cost moves. We always pay one atomic refcount op on heap-value assignment. **What we gain vs. tracing GC**: predictable, low-latency reclamation; no stop-the-world pauses; the runtime stays tiny.

### `Text` reference counting

Heap `Text` is fully reference-counted — both as a scalar (`||`, `read_line`) and as a relation record cell — alongside `Relation` payloads. The design rests on two facts and one invariant.

**Uniform headers.** Every `Text` value carries a `CoddlRcHeader`. The heap producers (`coddl_text_concat` / `coddl_char_to_text` / `coddl_read_line`, and SQLite text-column materialization) allocate one via `coddl_rc_alloc`; **string literals** are emitted by both codegen backends with an *immortal* header (`rc = IMMORTAL_RC`) ahead of their bytes, the payload pointer offset past it. So `coddl_rc_retain` / `coddl_rc_release` run safely on *any* `Text` — a literal sees the sentinel and no-ops.

**Owned vs borrowed provenance (scalars).** A `Text` SSA value is *owned* (a heap producer's result, or a retained alias) or *borrowed* (a `(ptr,len)` loaded out of a cell via `TupleField` / `Extract`). The lowerer tracks owned values in `owned_texts` and only auto-releases those — at scope exit for owned locals, and right after a borrowing consumer (`||`, `coddl_text_eq`, a builtin call) for owned temporaries. Releasing a borrowed `Text` would be a premature free, so it is never done. **Exception — a field read out of a *boxed* tuple** (`AttrLoad`, [procir.md](procir.md) "boxed tuples") is **retained to an owned copy**: the box can be freed before the field is consumed (the field is returned, or the box was an owned-temp argument freed right after the call), so a bare borrow would dangle. A **relation/sequence** field read is likewise retained-to-own (boxed *or* a flattened tuple with a relation-valued attribute) — relations carry no owned/borrowed mark, so a borrowing consumer would otherwise over-release it.

**One-reference-per-cell invariant (relations *and* boxed tuples).** Every heap cell — `Text`, and now a **relation-valued attribute** — in a relation record or a boxed-tuple record holds exactly one reference owned by that record. It is established at *production*: a value stored into a cell is **retained on store** (backend `store_attr`); a cell copied from input relation(s) by a relop is **retained on copy** (`retain_text_cells`, before `seal`); a cell produced fresh at rc=1 is **moved in**; and the producing temporary of a fresh cell is released after the record is built (`release_call_arg_temp` over `tuple_cell_heap_temps`). Release is then **uniform** — the drop walker (`drop_relation_payload`) releases every heap cell of a record it drops (a relation cell via `coddl_rc_release` on the stored pointer; `Text` likewise). (Immortal-literal cells no-op throughout.) `tclose`'s intermediate dedups run over un-retained working copies, so they pass `release_dropped_text = false`; its final output is retained once.

**Flattened tuples own their cells too.** A tuple below the box threshold isn't a heap record — its cells pass as flattened per-attribute ABI operands (a `Text` as `(ptr,len)`, a relation as a pointer) — but it still owns **one reference per heap cell**, released when the tuple *value* dies (scope exit, or consumption into a larger owner: boxed on return, embedded in a `Relation` literal, materialized as `format` args). The lowerer tracks that ownership two disjoint ways: a fresh **producer temp** consumed into a cell transfers its reference in and is listed in `tuple_cell_heap_temps` (released by `release_call_arg_temp` on drain); a cell that aliases a **`NameRef`** binding (a bound owned value, or a borrowed parameter) is **retained at construction** so the tuple's reference is independent of the binding, and the tuple is tracked in `flattened_heading_owners` and released by a **heading walk** (`TupleField` borrow + `Release`, recursing into nested flattened tuples). The `NameRef` retain is what makes the cell survive its binding's release when the tuple outlives it — a flattened tuple flowing out of an `if`-arm to a merge parameter after the arm frees the binding (otherwise a use-after-free → double-free, the web-response crash). An `if`-merge result parameter over a flattened tuple inherits ownership from an owning arm (its cells exist only at runtime, so it joins `flattened_heading_owners`); a field read still retains its own copy, so the heading-walk drop never invalidates a live field. Reassigning such a tuple across a loop back-edge or `if` merge is still deferred (T0076), like the relation/sequence carry.

**`extract` (and boxed-tuple unbox) defer the source.** `extract` copies a record's cells into a tuple as borrowed values, so the source relation's release is deferred to **function** scope exit (after every use of the borrowed fields) rather than freed immediately — the drop walker would otherwise dangle the fields. A small-tuple call result is `TupleUnbox`ed the same way: the boxed record is deferred-released after the borrowed flattened fields are consumed.

**Borrowing builtin consumers release their owned temp.** `write_line` releases an owned `Text` argument temporary after the call; symmetrically `write_relation` releases an owned relation argument temporary (a `where`/`extract`/call result, or a retain-on-read boxed-tuple relation field) — otherwise the temp leaks.

Two leak oracles enforce this: the runtime unit oracle (`crates/coddl-runtime/tests/leak_oracle.rs`) over relops + concat, and the **end-to-end** gate — `CODDL_LEAK_CHECK=1` makes a compiled program report a non-zero `LIVE_ALLOCATIONS` balance at shutdown, which the driver e2e suite asserts against (see [runtime.md](runtime.md) "Debug instrumentation").

## Discipline (defaults — push back if a proposal conflicts)

These are the working assumptions that keep the model honest. They are *not* commandments — flag a conflict and we'll resolve it explicitly.

1. **No `&mut` / `&` / `Box` / `Rc` in the surface language.** What looks like "a reference to a tuple" in other languages is just "a tuple value" in Coddl. A method-receiver `self` is passed by value.
2. **No back-pointers in tuples or relations.** Already enforced by OO Pro 2.
3. **Closures capture by value.** Anonymous opers capture refcounted values; closing over a mutable local copies the *current* value, not the binding.
4. **No reference / box / pointer type at all.** Including indirectly via recursive type definitions that would let "a tuple containing a relation containing this tuple" exist. The typechecker rejects recursive type definitions; if we ever want them, we add cycle detection at the value-construction site rather than weakening the model.
5. **Mutating methods are surface sugar.** `customer.rename { new_name: "Bob" }` desugars to `customer := customer.rename { new_name: "Bob" };` in the caller's scope (with `customer` a `var`). The pure function returns a new value; the rebind happens at the call site. COW makes this cheap in the common case.

## When the model bends

Probably revisit if any of the following come true:

- Performance benchmarks show atomic-refcount overhead dominates in a realistic workload.
- A real use case needs shared mutable state (e.g. concurrent transaction coordination — though that's the runtime's responsibility, not the surface language's).
- Recursive type definitions turn out to be valuable enough to want them with value-level cycle detection.

In each case the path is: proposal → flag the conflict with this doc → resolve explicitly (change the model or find a way to express the use case within it). We don't silently grow GC machinery or lifetime annotations into the language.

## Languages we cherry-pick from

| From | Idea taken | Not taken |
|---|---|---|
| Rust | Sum types, pattern matching, expression-based blocks, formatter-as-tool | Borrow checker, lifetime parameters |
| Haskell | Pure functions by default, parameterised types, laziness as a per-type design choice (relations only) | Monadic IO, total laziness, type-level programming |
| Swift | ARC + COW + value types, method-style on free functions, sum types via enums with payloads | Class inheritance, protocol-oriented runtime polymorphism |
| Erlang | Per-scope arenas, all-values-passable, immutability default | Dynamic typing, the actor model in user space |
| Go | Simplicity, formatter-enforced style, no implicit conversions | `interface{}` escape hatch, `nil`, share-by-channel as the only concurrency tool |
| Clojure | Persistent data structures, REPL workflow | Dynamic typing, JVM coupling |
| OCaml | Pattern matching, sum types, eager evaluation | Module-system complexity, functor-heavy organisation |

The result reads like none of them because the *combination* — TTM relational core + Rust-style ADTs + Swift-style ARC + Erlang-style scope arenas + Haskell-style purity + Go-style simplicity — is genuinely its own thing.
