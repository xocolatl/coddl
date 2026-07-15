# Type system + typechecker

Coddl's type system follows TTM's RM Pre 1–10 (see [conformance.md](conformance.md)): scalars are named, finite sets of values disjoint from every other scalar type; `Tuple H` and `Relation H` are type generators with structural identity by heading; there are no nulls and no implicit coercion. This doc covers the design rationale, then the authoritative spec of what `coddl-types` enforces today: the `Type` representation, the built-in operator registry, every walk function, and every `T####` diagnostic code.

## Scalar types

A scalar type is a named, finite set of values disjoint from every other scalar type. Each user-defined scalar type carries one or more **possible representations** (possreps) — abstract representations made up of named, typed components — and a (possibly trivial) `CONSTRAINT` predicate that defines which possrep tuples denote real values of the type (RM Pre 4–5).

For every possrep `PR` of type `T` the system synthesizes:

- A **selector** of declared type `T`, one parameter per component (selector name = possrep name). Every value of `T` must be producible by an all-literal selector invocation.
- A **`THE_C` accessor** per component `C`: read-only in source position; pseudovariable in target position (`THE_C(V) := x` is sugar for `V := PR(…, x in slot C, …)`).

**Type constraints** (the `possrep`'s `constraint` predicate) are checked at every selector invocation — that's the sole choke point because values of `T` can only be constructed via the selector. Type-constraint violations are run-time errors; argument-type mismatches are compile-time.

### Built-in scalar types (v1)

`Integer`, `Rational`, `Approximate`, `Text`, `Character`, `Binary`, `Byte`, `Boolean`. PascalCase per [grammar.md](grammar.md) "Identifier case". Everything else — `Date`, `Timestamp`, `Uuid`, fixed-width numerics, decimal, currency — is a user-defined scalar type with one or more declared possreps. Coddl ships a small standard library of these definitions but they aren't built into the language. Each built-in has fixed mappings to (a) LLVM type via [procir.md](procir.md), (b) SQLite affinity + `CHECK` constraints where needed via [storage.md](storage.md), (c) Postgres type; user-defined scalars get their mappings via possrep components.

### Three numeric types

`Integer`, `Rational`, `Approximate` — with **no implicit conversion** between them. `Integer` is mathematically unbounded (a bignum at runtime); `Rational` is exact rational arithmetic (also potentially unbounded); `Approximate` is bounded-precision floating-point (maps to f64). The literal shapes (see [grammar.md](grammar.md) "Literals") pick one without inference: `42` is `Integer`, `42.0` is `Rational`, `42e0` is `Approximate`. Code that needs Approximate-cost arithmetic on integer values writes `42e0`, not `42`. Users who need a bounded fast integer (e.g. `Int32`, `Int64`) define it as a user-defined possrep-constrained scalar over `Integer` — though see [risks.md](risks.md) risk #9 for the open question of whether to add bounded built-ins.

### `Text` and `Character` are separate types

`Text` is an opaque character string — you cannot index into it (`t[2]` is a type error), cannot ask its length in code points without explicit conversion, and cannot pattern-match on its internal representation. `Character` is a single Unicode code point. A planned standard-library function (TBD spelling) converts `Text` to `Sequence Character` and back; that's the only sanctioned route between the two.

The split matches Rust's `String` / `char` distinction and TTM's Appendix-A "scalar is atomic" rule (`Text`'s opacity is what lets the backend store it as `TEXT` / `VARCHAR` / a hash, depending on workload, without leaking implementation through indexing). Coddl deliberately departs from TTM's `CHARACTER` (a.k.a. `CHAR`) shorthand-for-string convention — see TTM ch. 6 p. 134 — because Coddl needs the names for two distinct types.

`Binary` and `Byte` mirror the same opacity split. `Binary` is an opaque byte blob (`b[2]` is a type error). `Byte` is a single octet (0–255). The planned conversion functions are `oper to_bytes { self: Binary } : Sequence Byte` and `oper from_bytes { bytes: Sequence Byte } : Binary`, paralleling `to_codepoints` / `from_codepoints` on `Text`.

### No implicit coercion; static operator overloading

Distinct named scalar types are disjoint; `Integer` and `Rational` cannot be silently mixed. Equality `=` is type-monomorphic per RM Pre 8 ("indistinguishable for all operators on T").

**No coercions — TTM's rule, applied.** The Manifesto defines coercion as the *implicit* invocation of a conversion operator to repair a wrongly-typed operand ("operands must always be of the appropriate types, not merely coercible to those types" — ch. 3, p. 74), and Coddl adopts the prohibition wholesale. What it does **not** prohibit is an operator whose *declared* signature mixes type categories — the Manifesto's own escape hatch: faced with the relation↔array crossing, Tutorial D's authors "prefer to define a new operation (LOAD), **with operands that are explicitly defined to be of different types**, instead of relying on conventional assignment plus coercion" (ch. 5, p. 123). Coddl's `load … from …` *is* that operation, and `when : Relation H × Boolean → Relation H` follows the same pattern (see [grammar.md](grammar.md)): its condition is required-Boolean and is-Boolean; the degree-0 relation in its semantics is IR-internal, never a converted operand. The discipline this pins down: **values never convert between `Boolean` and `Relation {}`** — the proscribed shapes stay hard type errors:

- `R times c` with a Boolean `c` is T0023, never auto-lifted to `R times ⟨c⟩` (that would be the ch. 3 `S# = 'S2'` shape verbatim);
- `c = reltrue` is a type error (comparands must be of the same type — same page);
- a `Boolean` never assigns to or passes as a `Relation {}`, nor vice versa.

Crossings between the two truth systems go only through operators declared across the boundary: `is_empty{}` and the relational comparisons one way, `when` the other.

**Static operator overloading is permitted.** A few comparison operators resolve to distinct underlying operators depending on the operand type family — most notably, `<=` and `>=` are scalar comparison on scalars and **subset** / **superset** on relations (`<` and `>` give strict subset / superset). The same identifier names two operators; the type checker picks which based on operand types at compile time. RM Pre 8 monomorphism is preserved because each underlying operator is type-monomorphic; the surface `<=` is just a shared spelling, the same way `+` can be spelled by `Integer` addition and `Rational` addition without violating RM Pre 8. The registry expresses this directly: a built-in name maps to a *list* of signatures, and the checker resolves a call by the static argument types — `to_text` (see [Built-in operator registry](#built-in-operator-registry)) is the first such overloaded builtin.

### No nulls

The type system has no nullable-attribute facility. Missing information is a database-design problem the user solves through **vertical decomposition** — splitting the relvar so the absence of a fact is the absence of a tuple in a side relvar (the canonical TTM answer; ch. 7, RM Pro 4). A user-defined sum-type scalar (`Optional` with `Some` / `None` possreps) is permitted by the type system but not the recommended approach. The SQL backend never sees a request to emit a NULL — see [sqlemit.md](sqlemit.md).

## Type generators

- `Tuple { a: T, b: U, … }` and `Relation { a: T, b: U, … }` are type generators producing structurally-identified types: `Tuple H1 = Tuple H2` iff `H1 = H2` as sets of `{name: type}` pairs. Same for `Relation`. Attribute order is immaterial. Both generators may take zero attributes (`reltrue` and `relfalse` are the only inhabitants of `Relation {}` — see the naming note below).
- **`Tuple {}` is the unit type** — the type of a tuple with no attributes. It has exactly one value, written `{}` (the empty tuple literal). This is Coddl's analogue of Rust's `()`, Swift's `Void`, or the unit type in ML. An `oper` declared without an explicit return clause implicitly returns `Tuple {}`. The two spellings `Tuple {}` and the value `{}` are unambiguous in context — one appears in type position, the other in expression position.
- `Sequence T` is the ordered counterpart — a finite ordered list of values of element type `T`, duplicates allowed, position significant. It's the procedural-side companion to `Relation`: where `Relation H` is an unordered set of tuples (RM Pro 1, 3), `Sequence Tuple H` is an ordered list of tuples (the canonical iteration form — see [runtime.md](runtime.md) "load"). The element type `T` may be any type — primitives (`Sequence Integer`), tuples (`Sequence Tuple H` — the typical case), or even relations (`Sequence Relation H`). The type generator `Sequence T` and its literal `Sequence [ … ]` are wired through the frontend (parse + typecheck); the literal is permitted only as a `let` binding value (T0063), and its element type is inferred from the elements or, when empty, taken from the `let` annotation (`let s: Sequence Integer = Sequence []`, else T0061). A non-empty literal **constructs** at runtime (an RC'd heap value, physically a kind-tagged unsealed relation over a synthetic single-attribute heading, so element storage and drop reuse the relation machinery). Still deferred to the `load` form: **iteration**, and constructing an **empty** literal (no element to derive the payload layout from — lowering emits T0064 for `Sequence []`).
- Headings may include relation-valued and tuple-valued attributes (nesting permitted; RM Pre 6–7).
- A **relvar** is a named variable of some `Relation H` type. Per RM Pre 14, every relvar has at least one declared candidate key (RM Pre 15), possibly the empty key (which forces cardinality ≤ 1). Coddl classifies relvars by lifetime and provenance, with one of the following kinds at declaration time:
  - **database relvars** (visible only in `.cddb` catalogs — see [plan.md](plan.md)): `real` / `base` (backed by storage), or `virtual` (a view).
  - **application relvars** (declared in `.cd` source): `private` (in-memory, lifetime of the program), or `public` (the program's view onto a slice of the database — see [storage.md](storage.md)).

  The same four-kind classification appears in Tutorial D (ch. 5 p. 105) because the underlying distinctions are real ones, not because we're copying it.

### Naming note: `reltrue` and `relfalse`

The two inhabitants of the type `Relation {}` (the nullary relation type — relation with empty heading) are called `reltrue` (cardinality 1, containing the empty tuple) and `relfalse` (cardinality 0, the empty relation). TTM and Tutorial D call them `TABLE_DEE` and `TABLE_DUM`, opaque even to readers who know TTM. Coddl renames them after their semantic role: `reltrue` is the multiplicative identity of the join semiring and behaves like boolean true under projection-away-of-everything; `relfalse` is the zero of the same semiring.

In terms of the type generators, the literal forms decode as:

- `relfalse` ≡ `Relation {}` (an empty relation literal — no tuples).
- `reltrue` ≡ `Relation { Tuple {} }` ≡ `Relation { {} }` (a relation literal containing the one and only empty tuple).

The `Relation { … }` syntax is contextual: in **type** position it's the type generator with a heading; in **value** position it's a relation literal whose body is a comma-list of tuple-valued expressions. The empty form `Relation {}` is the value form. The empty tuple `{}` may also be written `Tuple {}` in expression position — the `Tuple` constructor and the bare braced literal are equivalent for tuple values.

The names `reltrue` and `relfalse` themselves are **not compiler intrinsics**: they are ordinary module-level `let` bindings defined in the always-in-scope `coddl::core` (see [prelude.md](prelude.md) "Modules") — `let reltrue: Relation {} = Relation { {} };` and `let relfalse: Relation {} = Relation {};` — bare-available like `true`, shadowable by user bindings like any other name.

## Relations are fully first-class

Relations can be bound to variables, passed to and returned from operators, stored in tuples, nested inside other relations, used as function arguments and results everywhere a scalar can — subject to the lazy-evaluation semantics in [runtime.md](runtime.md). The calling convention treats them uniformly.

## The bottom type `Never`

`Never` is the **bottom type**: the type of an expression or block that never yields a value because control leaves it first. The producer is an early **`return`**. A block has type `Never` when **any statement diverges** — a bare `return`, or a statement-position `if/else` both of whose arms return (its own type is `Never`) — or when its **tail** is itself `Never` (e.g. a tail `if` both of whose arms return). Everything after a divergent statement is dead code; the block can't fall through to a value.

`Never` is **assignable to every type** (a diverging path can stand in wherever any value is expected) and **unifies as the identity** (`Never` with `T` yields `T`). This is what lets a `return`-only `if` arm agree with its value-producing sibling instead of tripping `T0068`, and a guard clause `if bad then [ return … ]` (no `else`) not trip `T0069`. An operator body that always returns is thus `Never`, which is assignable to any declared return type.

Like [`FormatText`](#format-and-the-formattext-firewall), `Never` is **unspellable** — it is absent from `from_builtin_name`, so no user `TypeRef` can name it; it is produced only by divergent control flow and never survives lowering (a diverging expression's value is never materialized — the lowerer hands back a `Unit` placeholder and a diverging `if`-arm feeds no merge argument). It is Coddl's analogue of Rust's `!` or Swift's `Never`, and is designed to extend cleanly to future diverging constructs (`break` / `continue` / error-raising operators). The early-`return` lowering — a mid-function `Return` terminator that unwinds every active scope's heap locals — is described in [procir.md](procir.md).

## Type inference and constraint inference

Type inference for relational expressions is mandatory and mechanical from operator semantics (RM Pre 18): every RelIR node's heading is the heading of its operands transformed by its operator. The optimizer further runs:

- **FD propagation** for candidate-key inference (VSS 3) — best-effort.
- **Constraint propagation** (RM Pre 23): predicates known to hold on operands propagate through restrict, project, join, extend, etc. Used for view-constraint checking and as optimizer hints.

## Where constraints can live

Integrity constraints attach only to **database relvars** (real, virtual). Coddl does not support constraints on application relvars (private or public), tuple variables, or scalar variables — there's "no logical reason why it should not," as TTM acknowledges (ch. 5 p. 106), but the cost in implementation complexity outweighs the payoff for the use cases we've identified so far. Revisit if a concrete need surfaces.

---

# Implementation spec

The rest of this doc pins what `coddl-types` enforces today.

**Last sync:** `94dfa9f`. Every commit that adds, removes, or changes a `T####` code, a built-in operator, or a typechecker walk method updates this file in the same commit; `tools/check-grammar.sh` enforces it from the hygiene gate.


## Type representation

The typechecker reasons in a flat `Type` enum. Two types are
*assignable* when they're structurally identical, or when at least one
is `Unknown`.

| Variant       | Meaning                                                  |
|---------------|----------------------------------------------------------|
| `Integer`     | Mathematical (unbounded) integer.                        |
| `Rational`    | Mathematical (unbounded) rational.                       |
| `Approximate` | Bounded-precision floating point.                        |
| `Text`        | Opaque character string.                                 |
| `Character`   | Single Unicode code point.                               |
| `Binary`      | Opaque byte blob.                                        |
| `Byte`        | Single octet (0–255).                                    |
| `Boolean`     | `true` or `false`.                                       |
| `Tuple H`     | Structural tuple over heading `H`.                       |
| `Relation H`  | Structural relation over heading `H`.                    |
| `Unknown`     | Sentinel used during error recovery; equals anything.    |

`Tuple` with an empty attribute set (`Tuple {}`) is the **unit type** —
the implicit return of every `oper` declared without an explicit
return clause.

### `Heading`

`Heading` is the structural shape shared by `Tuple H` and `Relation
H`. The constructor `Heading::new(fields)` sorts the attribute pairs
by name, so two headings declared with the same set in different
source orders compare equal; attribute lookup is a binary search.
This is what `RM Pro 1` (no ordinal position on attributes) buys
the typechecker.

### Deliberately not yet typed

The following are deferred until the relevant productions arrive:

- **`Sequence T`** is fully wired: the `Type::Sequence` variant, the
  `Sequence [ … ]` literal (empty and non-empty), `Sequence T` annotations,
  runtime construction (a `ProcType::Sequence` + `Inst::SequenceLit`
  lowering to an RC'd `CoddlKind::Sequence` value), **iteration** via
  `load … from … order [ … ]` (binds an unannotated `var` as its
  definite-assignment site and infers `Sequence Tuple { … }` from the source
  heading, then a `for … in` / counted loop walks it), and the **reverse**
  `load <private-relvar> from <sequence>` (seals the sequence's element
  tuples back into a relvar as a set — `Inst::Collect` →
  `coddl_relation_from_sequence`). An empty `Sequence []` takes its element
  type from the binding annotation (like an empty `Relation {}`); the former
  empty-construction gap (T0064) survives only as a defensive lowering
  fallback.
- **User-defined scalar types** via `possrep` — the **single-possrep,
  single-component** tier is implemented: `type Name { component: T };`
  declares a distinct nominal scalar (`Type::Scalar`, disjoint from `T` per
  RM Pre 1), with a synthesized selector `Name { component: e }` and possrep
  accessor `x.component`. It erases to its component in ProcIR (a
  single-possrep scalar *is* its component). Deferred: multi-component
  possreps (T0091), multiple possreps + author-supplied conversions
  (the `Point` cartesian/polar case), possrep `CONSTRAINT` predicates, and
  `THE_`-as-assignment-target. User-oper *parameter* types naming a scalar
  still resolve quietly to `Unknown` (the same pre-existing gap as aliases —
  the loud body path resolves them).
- **Function types** — `oper` signatures are stored in the built-in
  registry as `OperSig`, not as a first-class `Type` value.


## Relvar table

The typechecker's pre-pass populates a `RelvarTable` from every
declared relvar in the file: `public` / `private` in `.cd`, `base` /
`virtual` in `.cddb`. Each entry carries the relvar's kind, canonical
`Heading`, candidate keys (in source order; each inner `Vec<String>`
is one key's attribute names), and the span of the declaration's
name token for downstream "declared here" notes.

The table is exposed via `CheckOutput::relvars` so Phase 16's plan
layer can cross-validate `.cd` against `.cddb` and Phase 18+ can
expose entries to operator-body name resolution.

Key validation: every attribute named in `key { … }` must appear in
the heading; offenders emit `T0013`. Multi-key declarations (`key {a}
key {b}`) parse and each key validates independently — downstream
uses the first key for v1 (the typechecker stores them all).

Dialect legality: `public` / `private` are `.cd`-only; `base` /
`virtual` are `.cddb`-only. The parser of either dialect accepts all
four kinds so the typechecker can emit `T0014` at the name token
rather than producing a generic parse error.


## Built-in operator registry

The `Builtins` table maps operator names to a *list* of `OperSig`s
(most names have one; overloaded names have several). A call whose
callee is a `NameRef` looks up its lexeme in this table; an unknown name
produces `T0001`.

| Name             | Heading                            | Returns    |
|------------------|------------------------------------|------------|
| `write_line`     | `{ message: Text }`                | `Tuple {}` |
| `write_relation` | `{ rel: Relation H }`              | `Tuple {}` |
| `read_line`      | `{ prompt: Text }`                 | `Text`     |
| `to_text`        | `{ self: T }`                      | `Text`     |
| `cardinality`    | `{ self: Relation H / Sequence T }`| `Integer`  |
| `is_empty`       | `{ self: Relation H }`             | `Boolean`  |

`write_relation` is polymorphic over the heading `H` (see
"`write_relation` polymorphism" below). `read_line` is the first
`Text`-returning builtin. `write_line` also has a second, frontend-hardcoded
form `{ template: FormatText, args: Tuple H }` (not a registry row — resolved
like `format`); see "`format` and the `FormatText` firewall" below.

`cardinality` is overloaded over `Relation H` and `Sequence T` — the TTM
`COUNT` over a relation, and the element count of a sequence. Both store
their count in the same RC-header `length` slot, so it lowers to one
runtime read (`coddl_rc_length`), borrowing the receiver (it never alters
the refcount). Like `write_relation`, each overload is heading-/element-type
polymorphic — a dedicated `ParamKind` (`AnyRelation` / `AnySequence`)
accepts any heading or element type. `Text` is intentionally excluded: its
`length` is a byte count, not a character count.

`is_empty` (`Relation H`, pure) is true iff the cardinality is zero — a
compile-time convenience over `cardinality`. It has no runtime symbol: the
lowerer desugars `R.is_empty{}` to the same `coddl_rc_length` read compared
to `0` (`cardinality = 0`). Registered over `Relation H` only for now.

`to_text` is the first **overloaded** builtin: one monomorphic signature
per scalar type `T` — `Text` (an identity copy), `Character`, `Integer`
(decimal), and `Boolean` (`"true"`/`"false"`); the other scalar types
follow as the runtime grows. The checker resolves the call by the static
type of `self`
(no match → `T0054`); each underlying signature is type-monomorphic, so
RM Pre 8 holds (the shared spelling is what's polymorphic, not any one
operator). This is the same overload-by-argument-type machinery that
later serves UFCS `self`-dispatch and user-defined scalar operators.

### User-defined operators

A top-level `oper` declaration is callable like any built-in. Before any
operator body is walked, a pre-pass collects every user `oper`'s signature
(`name`, parameter headings, return type) into a table that is the operator
analogue of the relvar table — so a call resolves regardless of declaration
order (forward references are fine). A call's callee name is resolved in
this order: the `format` intrinsic, then user operators, then the built-in
registry, then `T0001`. To keep every name resolving to exactly **one**
definition, registration rejects a user `oper` whose name already names a
built-in (or `format`) or an earlier user `oper` with `T0060`, keeping the
first.

A user operator is checked through the same monomorphic path as a
single-signature built-in (`check_monomorphic_call`): the same signature
type (`OperSig`) carries both, with parameter names widened to `Cow` so the
built-in literals stay borrowed while user params own their strings. Every
user parameter is a `ParamKind::Concrete` type; missing/extra/mistyped
arguments still produce `T0003`/`T0004`. User operators default to
`SideEffecting` purity — the sound default for the transaction gate
(`T0026`) until body-derived purity lands (a pure helper is then
conservatively barred from a `transaction [...]`, never the reverse).
User-operator *overloading* (a second user overload of one name) is later
work.

### Imported operators (userspace modules)

A program is checked as a set of **units** — the entry `program`/`library`
plus every userspace `module` it transitively imports (`use module <leaf>;`),
resolved by the plan layer into a dependency-first graph (see
[plan.md](plan.md)). `check_program` checks each unit separately: a module's
body is genuine code (unlike a signature-only stdlib module), so it is
type-checked with its own imports in scope, and every diagnostic carries that
unit's `FileId` (so an error in `greet.cd` reports against `greet.cd`).

Scoping is **opt-in and per-unit**. A unit sees its own operators plus the
exported operators (all top-level `oper`s — there is no `pub` keyword yet) of
the modules it *directly* imports. Imports live in a table **separate** from the
unit's own operators, which gives two properties:

- **A local definition shadows an import.** Resolution consults builtins and the
  unit's own `oper`s first; the imported table is checked only when nothing local
  matches. So a unit may define its own `helper` even while importing a module
  that also exports `helper` — no `T0060`, and the call binds locally.
- **Same-named exports from two modules coexist until used.** Importing two
  modules that both export `greeting` is fine; only a *call* to the ambiguous
  name is an error (`T0092`), resolvable by defining a local `oper` of that name.

An un-imported module's operators are never in scope — the name stays a free
identifier the unit may define itself (the same discipline as the opt-in stdlib
modules). The single-file `check` entry point is `check_program` with one entry
unit and no imports, so unit-test fragments and the LSP's single-buffer path are
unchanged.

### Module-level `let` (constant bindings)

A `let` at module position is a **constant binding** — the same production as
the statement form (name, optional `: <type-ref>` annotation, `=`
initializer), with the module-scope rules carried by the *position*, not a
keyword. (`var` at module position is rejected at parse time, P0086 —
module-level mutable state is a relvar.)

- **Initializers are constant expressions**: literals,
  tuple/relation/sequence literals, built-in operators over them, and
  references to other module lets. Calls (until purity derivation),
  `transaction` blocks, relvar reads, field access, `if`, and indexing are
  **T0098**. A missing initializer is T0098 too.
- **Order-independent**, like every other module item (the sibling of the
  operator forward-reference rule): a syntactic prepass collects the names,
  the initializer reference graph is topologically ordered, and bindings
  check (and later fold / materialize) in dependency order — purity is what
  makes dependency order the only observable order. A reference cycle is
  **T0097**. Cross-module references follow the (already acyclic) module DAG.
- **The binding discipline is `check_binding`'s, at module scope**
  (`check_binding_rhs` is the shared core): an annotation is authoritative
  (T0010 on mismatch) and feeds empty constructor literals — a
  `let none: Relation { a: Integer } = Relation {};` is the headed empty
  relation; an unannotated empty `Relation {}` is relfalse; an empty
  `Sequence []` still requires the annotation (T0061).
- **Resolution order**: oper-locals shadow module lets (no-reserved-words
  discipline); module lets shadow imports; two imports exporting the same
  name coexist until used (**T0092**, the imported-oper rule). A module-let
  name may not reuse another module-level name (T0060). Last in the chain
  come the **always-in-scope stdlib lets** (`coddl::core`'s
  `reltrue`/`relfalse`): annotated by convention (the checker reads only the
  annotation — their signature, like a `builtin oper`'s heading), shadowable
  by any user binding, never consulted by the duplicate check.
- **Evaluation is once, never per use**: the lowerer folds scalar-typed
  bindings at **compile time** (checked Integer arithmetic, the runtime's
  Rational reduce/narrow rules — a folding failure is a T0098 compile error,
  never a silent wrap), so each use is one constant and a
  `where col = CONST` predicate pushes with the folded value; relation-typed
  bindings materialize once at startup into a slot riding the private-relvar
  machinery (stored by the synthesized `__coddl_module_lets_init`, released
  at shutdown), shared by importers. Under purity, *when* evaluation happens
  is unobservable. Lowerer v1 limits (both T0098): a tuple- or
  sequence-typed binding, and a relation-typed binding whose heading is
  neither annotated nor inferable from a direct relation-literal / module-let
  reference initializer.

### UFCS method calls

Any operator with a parameter literally named `self` — built-in or
user-defined — is callable in method position: `x.method { … }` is pure
sugar for `method { self: x, … }`. The braces distinguish it from a bare
possrep/field access (`x.field`). `check_call` handles it directly: when the
callee is a field access, it type-checks the receiver once, takes the field
as the method name, and injects the receiver as a synthetic `self` argument
into the existing monomorphic / overloaded resolution — so the receiver's
type participates in dispatch exactly like an explicit `self:` argument
(e.g. a `Sequence` receiver selects `cardinality`'s `Sequence` overload).
A method call on an operator with no `self` parameter is `T0070`; a receiver
whose type no overload accepts falls out as `T0004` (monomorphic) or `T0054`
(overloaded), the same codes a prefix call raises. Position of `self` in the
heading is irrelevant, and `self` never warns as unused.

### `format` and the `FormatText` firewall

`format { template: FormatText, args: <Tuple> } -> Text` is the string-
interpolation primitive. It is **not** in the registry: it is a compile-
time intrinsic (it needs a cross-argument check — every `{name}`
placeholder in `template` must name an attribute of `args` — and it has
no runtime symbol). It desugars to a `to_text`/`||` chain in lowering.

`FormatText` is the type of an `f"…"` literal (see
[grammar.md](grammar.md)). It is **compile-time-only** (never lowered — it
desugars away) and **non-storable** (it is unspellable as a type name, so
it can never be a relvar/tuple attribute). A template may appear inline as
`format`'s `template` argument **or** be bound once to a `let` and reused —
`let t = f"Hi, {name}!";` then `format { template: t, args: … }` at
several call sites. Only the two provenances qualify: a direct `f"…"`
literal, or a bare name reference to a `let` bound *directly* to one (`var`
templates, alias chains, and `if/then/else`-produced templates are
rejected — they'd make the provenance statically ambiguous). The template
is parsed once at its binding site; each `format` call validates its own
`args` against the shared chunks. Crucially there is still **no
`Text → FormatText` coercion**: that absence is the firewall keeping a
runtime `Text` (e.g. `read_line` input) out of a template slot — the
trusted-format-string pattern. A `FormatText` (literal or `let`-bound)
used anywhere but a `template` argument (`format`'s or `write_line`'s — see
below) is `T0055`; a `template` argument that is neither an `f"…"` literal
nor a `let` bound to one is `T0056`.
`args` is heading-
polymorphic (`ParamKind::AnyTuple`) and optional (absent ⇒ empty
heading). Placeholder checks: malformed template → `T0057`; a placeholder
with no matching `args` attribute → `T0058`; an `args` attribute no
placeholder uses → `T0059` (warning); a placeholder whose attribute type
has no `to_text` overload — built-in **or** user-defined — → `T0054`
— the same code a direct `to_text { self: … }` over that type raises,
since each `{x}` desugars to `to_text { self: x }`. The check (and the
lowerer's dispatch) resolve across built-in and user overloads, so a user
`to_text { self: T }` makes `{x : T}` renderable (e.g. a `Sequence Text`
once such an overload is in scope); only a type with *no* matching
overload (a bare `Tuple`/`Relation`, an un-extended `Sequence`) is T0054.

**`write_line`'s format overload.** Besides `write_line { message: Text }`,
`write_line` accepts `{ template: FormatText, args: Tuple H } -> Tuple {}` —
the same heading shape as `format`, but it *writes* the interpolated text
instead of returning it: `write_line { template: t, args: { … } }` is
equivalent to `write_line { message: format { template: t, args: { … } } }`.
It is frontend-hardcoded exactly like `format` — intercepted before registry
resolution, reusing `format`'s validation (the same `T0004` / `T0056` /
`T0058` / `T0059` / `T0054` diagnostics, and `T0055` never fires because the
`template` is validated inline) and keeping `write_line`'s side-effecting
purity (`T0026` inside a `transaction [...]`). The overload is selected by
the presence of a `template` argument; the `message` form never carries one,
so the two are disjoint. `FormatText` and the heading-polymorphic `Tuple H`
stay **internal** — a user `oper` cannot declare a parameter of either type
(that needs the heading polymorphism / specialization deferred in
[risks.md](risks.md) §6–7), so this convenience is limited to the built-in
`format` family for now.


## Pass overview

The typechecker walks the AST exposed by `coddl-syntax`. Walk methods
are named to mirror the parser's productions in `docs/grammar.md`:
each `parse_<x>` has a corresponding `check_<x>`.

- **`check_root`** — two-pass over `.cd` items. The pre-pass collects
  every relvar declaration into the `RelvarTable` (via
  `check_public_relvar_decl` / `check_private_relvar_decl` /
  `check_base_relvar_decl` / `check_virtual_relvar_decl`); the main
  pass walks `check_program_decl` and `check_oper_decl`. The
  separation lets Phase 18+ resolve relvar references in operator
  bodies against a complete table.
- **`check_cddb_root`** — single pre-pass over `.cddb` items
  collecting every relvar declaration into the table. T0014 fires
  on `public` / `private` declarations (those belong in `.cd`).
- **`check_public_relvar_decl` / `check_private_relvar_decl` /
  `check_base_relvar_decl` / `check_virtual_relvar_decl`** — each
  resolves the heading into a canonical `Heading`, validates each
  candidate key's attributes (`T0013`), and inserts a `RelvarInfo`
  into the table (`T0012` on duplicate name). All four delegate
  dialect-legality checking to `is_kind_legal_for_dialect`, which
  emits `T0014` on a kind not legal in the current file's dialect.
- **`check_program_decl`** — no-op today; the program name is a label
  the runtime may use later. No semantic constraints yet.
- **`check_oper_decl`** — resolves the heading into a parameter
  scope (rejecting duplicate names with `T0007`), resolves the
  declared return type from the optional `-> <type-ref>` clause
  (defaulting to `Tuple {}`), then checks the body against the
  scope. If the operator's name is `main`, its heading must be
  empty (`T0006`) and its declared return must be `Tuple {}`
  (`T0011`) — the runtime always exits with `i32 0`, so a declared
  non-Unit return on `main` would lie about what the program
  produces. The body's result type from `check_block` must match
  the declared return; otherwise `T0009`.
- **`resolve_type_name`** — maps a `TypeRef`'s identifier to a built-in
  `Type`. Unknown names produce `T0005` and resolve to `Type::Unknown`.
- **`check_block`** — walks statements (let bindings update the
  scope's top layer; expression-statement results are discarded),
  then returns the tail expression's type (or `Tuple {}` if the
  block has no tail expression). The surrounding scope is a stack
  of binding maps — outermost is the operator parameter layer; each
  `transaction [...]` block pushes a layer on entry and pops on
  exit. Lookups walk innermost-first so inner bindings shadow
  outer ones. A block whose control flow leaves via a `return` — any
  statement whose type is `Never` (a bare `return`, or a
  statement-position `if/else` both of whose arms return), or a tail
  whose own type is `Never` — has type **`Never`**: it never falls
  through to a value (see "The bottom type `Never`" below). The
  per-statement type comes from `check_stmt`, which returns `Never`
  for a `return` and the expression's type for an expression-statement.
- **`check_if_expr`** — `if <cond> then [ … ] else [ … ]`. The
  condition must be `Boolean` (`T0067`). Each arm is a `check_block`
  in its own pushed scope (like a `transaction` body, but without the
  transaction-depth bump — an `if` is not a transaction boundary). With
  `else`, the two arm types must unify (`T0068`) and that is the
  expression's type; a **`Never`** arm (one ending in `return`) unifies
  as the identity, so the `if` takes its sibling's type (both `Never` ⇒
  `Never`) — this is what lets a guard's value-route sit opposite an
  `else [ return … ]`. Without `else`, the then-arm must be Unit
  (`Tuple {}`) or `Never` (a guard clause `if bad then [ return … ]`),
  else `T0069`; the expression's type is Unit — the statement form.
  `else if` is spelled by nesting an `if` in the `else` block.
- **`check_return_stmt`** — `return [<expr>];`, an early exit from the
  enclosing operator body. The value (or `Tuple {}` for a bare
  `return;`) is checked against the operator's declared return type,
  stashed while the body is walked; a mismatch is `T0018`. A `return`
  lexically inside a `transaction [...]` is `T0093` (its early exit
  would skip the commit). The statement itself has no value; it makes
  its enclosing block `Never` (see below).
- **`check_let_stmt`** — infers the RHS expression's type. If the
  binding carries an explicit `: <type-ref>` annotation, the
  declared type is authoritative: the RHS must conform (or `T0010`
  fires), and subsequent NameRef lookups see the *declared* type,
  not the inferred one. Without an annotation, the inferred type
  is bound. Shadowing is silently allowed.
- **`check_expr_stmt`** — calls `check_expr` on the embedded expression
  and discards the result.
- **`check_expr`** — returns the expression's `Type`. Dispatches on
  the `Expr` variant:
  - `NameRef` looks up the name in the surrounding scope stack.
    Unresolved names produce `T0001`.
  - `Literal` returns the type implied by the underlying token kind
    (`STRING_LIT` → `Text`, `CHAR_LIT` → `Character`, the three
    numeric kinds to the matching numeric type).
  - `Call` is `check_call`.
  - `Transaction` is `check_transaction_expr`.
  - `TupleLit` is `check_tuple_lit`.
  - `RelationLit` is `check_relation_lit`.
  - `FieldAccess` is `check_field_access`.
  - `BoolLit` types as `Boolean`.
  - `Binary` is `check_binary_expr`.
  - `Unary` is `check_unary_expr`.
- **`check_transaction_expr`** — pushes a scope layer, walks the
  body with `check_block`, pops the layer, and returns the body's
  result type.
- **`check_call`** — the callee must be a `NameRef` whose lexeme is in
  the built-in registry (`T0001` otherwise). Each named argument
  is checked by `check_named_arg`. After the arguments, every
  declared parameter must have been supplied (`T0003`).
- **`check_named_arg`** — recognizes duplicate argument names
  (`T0008`), arguments not declared by the operator (`T0002`), and
  type mismatches between the argument's expression and the
  parameter's declared type (`T0004`). When the argument matches a
  declared parameter, it's marked provided.
- **`check_tuple_lit`** — walks each field's value expression in
  source order, collecting `(name, Type)` pairs. Duplicate field
  names emit `T0015` and the second occurrence is dropped from the
  heading. The collected pairs flow into `Heading::new` (which sorts
  them into canonical order); the result is `Type::Tuple(heading)`.
  Empty `{}` types as `Type::unit()` — the generalized way to write
  the unit value at expression position.
- **`check_field_access`** — typechecks the base expression. If the
  base isn't a `Type::Tuple(_)`, emits `T0016` and returns
  `Type::Unknown`. Otherwise consults `Heading::lookup` for the
  field name; on miss emits `T0017` and returns `Type::Unknown`; on
  hit returns the attribute's type.
- **`check_relation_lit`** — walks each nested tuple via
  `check_tuple_lit`. An empty `Relation {}` takes its heading from an
  `expected: Option<Heading>` (a `Relation { H }` `let`/`var`
  annotation, threaded by `check_binding` — a **headed** empty
  relation), else defaults to `relfalse` — the nullary empty relation
  (∅ heading, zero tuples). Unlike an empty `Sequence []` (T0061), no
  annotation is *required*: `relfalse` is a sensible unconstrained
  default, and its sibling `reltrue` is `Relation { {} }`. A non-empty
  literal ignores `expected` and infers from its tuples: the first
  tuple's heading establishes the relation's heading; every subsequent
  tuple must match (per `Heading::assignable_to`), mismatches emit
  `T0019` on the offending tuple without cascading. Returns
  `Type::Relation(h)`.
- **`resolve_type_ref`** now resolves the three generator forms:
  `Sequence T`, `Tuple { H }`, and `Relation { H }` (headings via
  `resolve_heading`, recursively). The static `resolve_type_ref_quiet`
  is its no-diagnostic twin, exposed for the ProcIR lowerer (which
  resolves a `let`/`var` annotation's heading — and an operator's
  `Tuple`/`Relation` parameter/return heading — into a `ProcType`). Every
  signature shape is now supported: `Tuple`/`Relation` parameters and
  `Tuple`/`Relation` returns. A relation (and a **large** tuple) crosses
  the ABI as one pointer; a **small** tuple flattens per-attribute for
  passing and is boxed at the return boundary (see [procir.md](procir.md)
  "boxed tuples" and [codegen.md](codegen.md)). `T0018` is retired.
- **`write_relation` polymorphism** — the built-in's `rel`
  parameter has `ParamKind::AnyRelation` rather than a concrete
  type. `check_named_arg` special-cases this kind: any
  `Type::Relation(_)` matches regardless of heading; non-relation
  args emit `T0004`. The per-call-site heading is carried through
  the lowerer's `value_types` map and into the backend via
  `Inst::WriteRelation`'s `heading_id` field.
- **`check_binary_expr`** — dispatches on the parsed `BinaryOp`:
  - **Comparison (`=`, `<>`)**: polymorphic (static overloading, above).
    On scalars, operands must share a scalar type (Integer, Text,
    Character, Approximate, Rational, or Boolean for v1); T0021 on
    mismatch. `Approximate` `=` is canonicalized bit-equality (NaN → one
    value, `−0.0` = `+0.0`), not IEEE `oeq` — so it stays reflexive for
    dedup/keys (RM Pro 3). `Rational` `=` compares the reduced
    `(numer, denom)` pair (canonical lowest-terms form ⇒ value-equality).
    On **relations**, `=` is observational set equality (RM Pre 8:
    heading + tuple set, however the operands were built or fetched —
    never a payload/pointer compare); identical headings required, T0038
    (the `union`/`minus` rule) on a mismatch. Lowering: the runtime
    comparator checks cardinalities off the RC headers (free), then a
    content-aware membership walk (`record_cmp` — the same cell
    comparators seal/dedup use, so Text compares bytes, tuples recurse) —
    two equal-size duplicate-free sets with one containing the other are
    the same set. Result is `Boolean`.
  - **Ordering (`<`, `>`, `<=`, `>=`)**: polymorphic. On scalars, both
    operands must be the same scalar type — two Integer or two Rational
    (no mixing); T0021 otherwise. `Rational` ordering routes through the
    runtime's cross-multiply comparator (`a/b ⋛ c/d ⟺ a·d ⋛ c·b`, both
    products fitting the i128 intermediate), never lexicographic text
    order. On **relations**, `<=`/`>=` are subset/superset and `<`/`>`
    the strict forms (identical headings required, T0038) — there is no
    separate `subset` keyword. A proper subset must be strictly smaller,
    so both forms reject on cardinality before reading a record. Result
    is `Boolean`. In-process only for now: a comparison over pushable
    operands forces each as its own query, then compares — the
    Boolean-result pushdown channel (`NOT EXISTS (… EXCEPT …)`) is
    future work, and a *bare* public-relvar operand trips the S1
    full-pull guard rather than silently hydrating the table.
  - **Logical (`and`, `or`)**: both operands must be Boolean.
    Result is `Boolean`. T0021 otherwise. (Prefix `not` is the unary
    sibling — see `check_unary_expr` below, same T0021.)
  - **Arithmetic (`+`, `-`, `*`, `/`)**: both operands must be
    Integer (integer division truncates toward zero). Result is
    `Integer`. T0043 otherwise.
  - **Concatenation (`||`)**: each operand must be Text or
    Character (any mix). Result is always `Text` (two Characters
    can't be one Character). T0044 otherwise.
  - **`where`**: lhs must be `Relation H` (T0023 if not). A fresh
    scope layer is pushed with the heading's attributes as
    bindings, then the rhs (predicate) is checked; the predicate
    must be `Boolean` (T0020 otherwise). The scope is popped after
    the predicate. Result is `Relation H` (lhs's type unchanged).
  - **`when`** (`check_when_binary`): lhs must be `Relation H` (T0023).
    The condition is checked in the **ordinary enclosing scope** — no
    heading injection, the deliberate contract with `where` — and must
    be `Boolean` (T0099 otherwise; a relation-typed condition gets the
    `times` suggestion). If the condition names an unresolved identifier
    that *is* an attribute of `H`, a second T0099 points at `where` —
    the where/when confusion caught at its source. Result is
    `Relation H`.
  - **`otherwise`** (`check_otherwise_binary`): both operands must be
    relations (T0023) with **identical headings** (T0038, the
    `union`/`minus` rule). Result is `Relation H` — the shared heading.
- **Capture deferral (T0022)** — Phase 20 deferred capture support
  for `where` predicates. The typechecker's scope lookup walks
  innermost-first so an outer let binding would technically
  resolve; the **lowerer** detects this case (NameRef misses in
  the predicate's heading-scope but hits the saved enclosing
  scope) and emits T0022. Future phase will lift this restriction
  via a user_data pointer threaded through `coddl_relation_where`.
- **`check_unary_expr`** — dispatches on the parsed `UnaryOp`.
  `Extract`: the operand must be `Type::Relation(H)`; the result is
  `Type::Tuple(H)`; T0024 on a non-relation operand. `Not` (prefix
  `not` / `¬`): the operand must be `Boolean`; the result is always
  `Boolean` — returned even on a non-Boolean operand (T0021, shared
  with `and`/`or`) so the error doesn't cascade into the enclosing
  `if`/`and`/`or`. Other unary ops slot in here.
- **`check_project_expr`** — `R project { a, … }`. The operand must be
  `Type::Relation(H)` (T0023, shared with `where`). Each projected
  attribute must exist in `H` (T0027) and appear at most once (T0028);
  offenders are reported and dropped (best-effort recovery). The result
  is `Type::Relation(H')` where `H'` is `H` narrowed to the kept
  attributes, canonically re-sorted — so the source order of the names is
  irrelevant. Projection is well-typed regardless of where the relation
  came from; the **lowerer** serves a relvar-rooted operand by pushing the
  projection into SQL (a narrowed `SELECT`) and an in-memory operand with
  the in-process `Inst::Project` → `coddl_relation_project`.
- **`check_replace_expr`** — `R replace { new: e, … }` (compute-and-consume).
  Adds each `new` attribute bound to the computed value `e` and removes the
  operand attributes `e` references. The operand must be `Type::Relation(H)`
  (T0023). **Every value must compute**: a **bare attribute reference** only
  relabels, so it is rejected → use `rename` (**T0047**). A **general (compute)
  expression** is typechecked in a scope with `H`'s attributes injected (the
  same machinery `where`/`extend` use), so it may reference the operand's
  attributes; let `R = attr_refs(e) ∩ H` be the operand attributes it reads. `R`
  must be non-empty (a constant or a value reading no operand attribute removes
  nothing → use `extend`, T0042), and the value's type is restricted to
  **Integer or Text** (T0046, the same cell-type restriction `extend` enforces).
  The value adds `new` and consumes `R`. The result heading `H'` = the survivors
  (`H` minus every consumed source) plus each added `new`, with a new name
  colliding with a survivor or another target firing T0031, canonically
  re-sorted. Lowering desugars each pair through `extend` + `project` (all-but
  the consumed attrs) + `rename` (only when the new name collides with a
  surviving attribute, via an internal `__coddl_replace_tmp_*` temp). A
  relvar-rooted replace pushes to SQL as `SELECT (e) AS new` with the consumed
  columns absent (see the sqlemit peel-chain); an in-memory operand lowers to the
  in-process `Inst::Extend`/`Inst::Project`/`Inst::Rename` chain.
- **`check_rename_expr`** — `R rename { new: old, … }` — relational rename
  (relabel), the strict relabel-only partition of `replace`. The operand must be
  `Type::Relation(H)` (T0023). Each value must be a **bare attribute reference**
  `old`; a computed value is rejected → use `replace` (**T0030**). `old` must
  exist in `H` (T0029) and the rename must stay a bijection — no source relabeled
  twice, no target colliding with a surviving attribute (T0031). The result
  heading `H'` = `H` with each `old` relabeled to `new`, canonically re-sorted
  (type- and cardinality-preserving). A relvar-rooted rename pushes to SQL as
  `SELECT old AS new` (the `Rename` peel-chain); an in-memory operand lowers to
  `Inst::Rename` → `coddl_relation_rename`. (`rename`/`replace`/`extend` form a
  clean trichotomy: relabel / compute-and-consume / compute-and-keep.)
- **`check_wrap_expr`** — `R wrap { t: { a, b }, … }` — group attributes into
  tuple-valued attributes. The operand must be `Type::Relation(H)` (T0023). Each
  wrapped attribute must exist in `H` (T0027) and be wrapped at most once across
  all pairs (T0028); each new name must be fresh vs. survivors and other new
  names (T0031). Result heading = the un-wrapped survivors plus each
  `new : Tuple(<components with their H types>)`. (`wrap { t: {} }` → `t : Tuple {}`
  is allowed.) A relvar-rooted wrap declines the SQL push (Chunk-2: no emission)
  and restructures in-process via `Inst::Restructure` → `coddl_relation_restructure`.
- **`check_unwrap_expr`** — `R unwrap { t, … }` — expand tuple-valued attributes
  back to their components, lifted to top level (the inverse of `wrap`). The
  operand must be `Type::Relation(H)` (T0023). Each named attribute must exist
  (T0027), be listed once (T0028), and be `Type::Tuple(_)` (**T0048**); a lifted
  component colliding with a survivor or another lifted component is T0031.
  Result heading = the survivors plus each unwrapped tuple's components. Same
  in-process lowering as `wrap`.
- **`check_group_expr`** — `R group { pq: { a, b }, … }` — TTM GROUP: consume
  attributes into relation-valued attributes; the attributes named in NO pair
  survive and partition the relation (one result tuple per distinct survivor
  combination). The operand must be `Type::Relation(H)` (T0023). Each consumed
  attribute must exist in `H` (T0027) and be consumed at most once across all
  pairs (T0028); each new name must be fresh vs. survivors and other new names
  (T0031). Multi-pair `group` is **simultaneous** — one partition by the common
  survivors, each pair nesting its own components (`{…}` is unordered, so
  Tutorial D's sequential commalist is out; chain `group {…} group {…}` for
  sequential). Result heading = the survivors plus each
  `new : Relation(<components with their H types>)`. (`group { g: {} }` →
  `g = reltrue` per tuple — TTM exercise 2.34 — is allowed.) `group` never
  pushes to SQL (a relation-valued cell has no flat-column form): the operand
  fetch pushes at its own root and the nest runs in-process via `Inst::Group` →
  `coddl_relation_group`.
- **`check_ungroup_expr`** — `R ungroup { pq, … }` — TTM UNGROUP: unnest
  relation-valued attributes back to top level, one result tuple per
  combination of an outer tuple and one tuple from each named RVA (an empty
  RVA contributes nothing). The operand must be `Type::Relation(H)` (T0023).
  Each named attribute must exist (T0027), be listed once (T0028), and be
  `Type::Relation(_)` (**T0100** — the RVA analogue of unwrap's T0048); a
  lifted attribute colliding with a survivor or another lifted attribute is
  T0031 (rename before ungrouping). Result heading = the survivors plus each
  ungrouped relation's attributes. Same never-pushes lowering as `group`, via
  `Inst::Ungroup` → `coddl_relation_ungroup` (the output seals — unnesting can
  produce duplicates).
- **`check_extend_expr`** — `R extend { c: e, … }`. Adds each new attribute `c`
  bound to the computed value `e`, keeping every operand attribute (the dual of
  `replace`). The operand must be `Type::Relation(H)` (T0023). Each value `e` is
  a general scalar expression typechecked in a scope with `H`'s attributes
  injected (the same machinery `where` uses), so it may reference the operand's
  attributes. The new name `c` must not collide with an existing attribute or
  another `extend` target (T0045). Each value's type is restricted to **Integer
  or Text** — the only types representable as relation cells in v1 (T0046
  otherwise; Boolean/Character and non-scalar values await wider cell support).
  The result is `Type::Relation(H')` where `H'` is `H` plus each `(c, type_of
  e)`, canonically re-sorted. A relvar-rooted operand pushes to SQL (a computed
  `(<e>) AS "c"` column); a materialized operand (relation literal / private
  relvar) computes the column **in-process** — the lowerer synthesizes a
  per-tuple helper `fn(src_record, dst_record)` that writes the widened record,
  driven by the runtime's `coddl_relation_extend`.


## Transaction-scoped public-relvar access (Phase 22)

TTM OO Pre 4 forbids autocommit: every database access happens inside
an explicit transaction. The typechecker enforces this at every public-
relvar reference site.

- The `TypeChecker` carries a `transaction_depth: usize` counter.
  `check_transaction_expr` bumps it before the body, decrements after.
- `check_oper_decl` seeds the operator-body scope with every public
  relvar in the per-file `RelvarTable` as `Type::Relation(heading)`,
  and parallel-tracks them in a `public_relvars: HashSet<String>`.
- A `NameRef` that resolves to a public relvar AND finds
  `transaction_depth == 0` fires `T0025`. The diagnostic anchors on
  the name token.
- Private relvars are not in the set — RM Pre 14 calls them local
  program state, not database state, so the rule doesn't apply.

## Transaction purity (Phase 22)

Transactions must be replayable on serialization conflict. Anything
that touches the outside world (stdout, stderr, the network, a file
that isn't the materialized SQLite payload) is unsafe inside.

- `Builtins::OperSig` carries a `Purity` field (`Pure | SideEffecting`).
- Existing side-effecting builtins: `write_line`, `write_relation`,
  `read_line` (reads stdin — also barred inside a transaction).
  All future builtins default to `Pure` in the registry and must
  opt in to `SideEffecting` explicitly — adding a printing operator
  is a forcing function on the conformance check.
- `check_call`: when `transaction_depth > 0` and the resolved callee
  is `SideEffecting`, emit `T0026` at the callee token.
- The rule applies recursively through nested `transaction [...]`
  blocks (they only ever increase depth) and will extend to user-
  defined `oper`s once those callees carry a derived purity flag.

A relational assignment to a public relvar (`R := …`) is a DML statement and is
**not** caught by this ban. The T0026 purity rule targets side-effecting
*operator calls* (`Expr::Call` to a `SideEffecting` builtin) — non-transactional
I/O that can't be rolled back or replayed. A write to a public relvar is the
*legitimate, transactional* effect a `transaction [...]` exists to commit; it is
rolled back cleanly on conflict and replayed with the block. So it is allowed
inside a transaction (and, like any public-relvar access, is *required* to sit
inside one by T0025).

## Writing public relvars

Relational assignment `R := <expr>;` (`check_assignment_stmt`) accepts a public
*or* private relvar as its target. A private target stores into an in-memory
slot; a public target is a write to its SQL-backed table. The checker enforces
that the target is a bare name bound to an assignable relvar (**T0033**
otherwise) and that the RHS heading matches the target's (**T0034**). A
public-relvar reference on the RHS forces a `transaction [...]` (**T0025**).

The checker does not constrain the RHS *shape*: which assignments become
surgical DML — and which are rejected as not-yet-writable — is decided in the
lowering layer, where the RHS `RelExpr` is recognized (`R minus (R where …)`
→ `DELETE … WHERE …`; `R minus R` → a whole-table delete). Two checks live there
because they need information the `.cd` checker lacks:

- a public relvar mapped to a catalog **view** is not directly writable (the
  base-vs-virtual `WritePolicy` distinction; the checker only knows `Public` vs
  `Private`) — **T0050**;
- an RHS shape the backend cannot emit as surgical DML — **T0049**.

### Statement-verb sugar

The DML statement verbs are sugar over relational assignment; each desugars in
the lowerer to a recognized RHS shape, so the checker only validates the surface
and lets the lowering layer reuse the assignment machinery (`require_public_write`
→ T0050/T0049, `emit_assignment`). The verb checks (`check_truncate_stmt`,
`check_delete_stmt`) reuse the assignment's relvar-target resolution (**T0033**)
and transaction rule (**T0025**):

- **`truncate R;`** → `R := R minus R`. The operand must be a bare assignable
  relvar; a restricted or compound operand is **T0033**.
- **`delete R where p;`** → `R := R minus (R where p)`. The operand must be a
  `where`-restriction over a bare assignable relvar (**T0033**); the predicate is
  validated like any `where` (Boolean, **T0020**; heading scope-injected). The
  `where` is *mandatory* — a bare `delete R;` would clear the whole relvar, so it
  is **T0052** (pointing at `truncate`), keeping the verbs a clean partition
  (`truncate` = all, `delete` = matching).
- **`insert R <source>;`** → `R := R union <source>`. The target must be a bare
  assignable relvar (**T0033**) and the source a relation whose heading matches
  the relvar's (**T0034**). The two surface forms — a brace `<tuple-set>` (a
  keyword-less relation literal) or a relation `<expr>` — are a single `source`
  expression to the checker, so one `check_expr` validates either (an empty
  tuple-set is `relfalse`, the nullary empty relation, whose ∅ heading a headed
  relvar rejects as a heading mismatch, **T0034**).
- **`update R where p { c: e };`** → `R := (R where ¬p) union ((R where p)
  «sub»)`. The operand must be relvar-rooted (a bare relvar, or `R where p`) over
  a bare assignable relvar (**T0033**); the predicate is validated like any
  `where` (Boolean, **T0020**; heading scope-injected). Each `{ c: e }` target
  must be an **existing** attribute (**T0053** — `update` overwrites, it doesn't
  add) whose type the value matches (**T0034**), and no target is named twice
  (**T0031**). Unlike `replace`, the values may be constants or bare references —
  T0042/T0047 are *not* applied (the scope-injection and per-pair checks reuse
  `check_replace_expr`'s, minus those two surface lints).


## Typecheck diagnostics

Every diagnostic the typechecker emits has a stable `T####` code.
Every code in `crates/coddl-types/src/` appears here; the hygiene-
check script enforces that.

| Code  | Trigger                                                  |
|-------|----------------------------------------------------------|
| T0001 | Cannot resolve name (unknown callee, unbound `NameRef`)  |
| T0002 | Argument is not declared by the called operator          |
| T0003 | Missing required argument in call                        |
| T0004 | Argument type mismatch (expected vs. supplied)           |
| T0005 | Unknown type name in a `TypeRef`                         |
| T0006 | `main` must take zero parameters                         |
| T0007 | Duplicate parameter name in heading                      |
| T0008 | Duplicate argument name in call site                     |
| T0009 | Operator body's result type doesn't match its declared return |
| T0010 | `let` binding's declared type doesn't match the RHS       |
| T0011 | `main` must return `Tuple {}` (the unit type)            |
| T0012 | Duplicate relvar — name already declared in this file    |
| T0013 | Key attribute is not in the relvar's heading             |
| T0014 | Relvar kind is not legal for this dialect (`public`/`private` in `.cddb`, `base`/`virtual` in `.cd`) |
| T0015 | Duplicate field name in tuple literal                    |
| T0016 | Field access on a value whose type isn't a tuple         |
| T0017 | Unknown field name in tuple field access                 |
| T0018 | a `return` value's type doesn't match the enclosing operator's declared return type (a bare `return;` requires a Unit-returning oper). *(Reused: formerly gated a `Tuple`/`Relation` heading in an operator signature — that shape now lowers, so the code was retired and re-allocated here.)* |
| T0019 | Tuple heading mismatch in relation literal                |
| T0020 | `where` predicate must be Boolean                         |
| T0021 | Comparison/logical operand type mismatch (scalars must share a type; comparisons alternatively take two same-heading relations) |
| T0022 | Captured identifier in `where` predicate not yet supported |
| T0023 | `where` / `when` / `otherwise` / `project` / `replace` left operand is not a relation (`otherwise` checks both operands) |
| T0024 | `extract` operand is not a relation                       |
| T0025 | Public relvar referenced outside any `transaction [...]`  |
| T0026 | Side-effecting operator called inside `transaction [...]` |
| T0027 | Unknown attribute name in a `project` list                |
| T0028 | Duplicate attribute name in a `project` list              |
| T0029 | Unknown attribute name in a `replace`/`rename` source (the value side) |
| T0030 | `rename` value is not a bare attribute reference (a computed value) — use `replace` |
| T0031 | `replace`/`rename` is not a bijection (a source removed twice, or a target collides with a surviving attribute) |
| T0032 | *(warning)* unused `let` binding or parameter — never referenced, not `_`-prefixed (a `self` parameter is exempt) |
| T0033 | relational-assignment target is not an assignable (private) relvar (not a relvar name, or a read-only public relvar) |
| T0034 | relational-assignment RHS does not match the target relvar's heading |
| T0035 | `join`/`compose` operands share no attribute (disjoint headings) — suggest `times` |
| T0036 | `join`/`compose`/`matching`/`not matching` shared attribute has different types on each side |
| T0037 | `times` operands share an attribute (overlapping headings) — suggest `join` |
| T0038 | `union`/`intersect`/`minus`/`otherwise` — and the relation comparisons `=`/`<>`/`<=`/`>=`/`<`/`>` — operands must have identical headings |
| T0039 | `join` operands have identical headings (the join is a set intersection) — suggest `intersect` |
| T0040 | `compose` operands have identical headings (every attribute removed, result always nullary) — suggest `intersect` |
| T0041 | `tclose` operand must be a relation of exactly two attributes of the same type (binary graph relation) |
| T0042 | `replace` value references no attribute, so it removes nothing — use `extend` to add without removing |
| T0043 | arithmetic operator (`+`, `-`, `*`, `/`) requires Integer operands |
| T0044 | `||` requires Text or Character operands |
| T0045 | `extend` attribute already exists / duplicate `extend` target |
| T0046 | computed `extend` / `replace` value must be Integer or Text (v1 relation-cell support) |
| T0047 | `replace` value is a bare attribute reference (only relabels) — use `rename` |
| T0048 | `unwrap` target is not a tuple-valued attribute                    |
| T0049 | assignment to a public relvar has an RHS shape the backend cannot emit as surgical DML (lowering) |
| T0050 | assignment target is a public relvar mapped to a non-writable view (lowering) |
| T0051 | _(warning)_ `R := R` self-assignment has no effect (elided) |
| T0052 | `delete` without a `where` clause (a bare `delete R;`) — use `truncate` to clear the whole relvar |
| T0053 | `update` clause names an attribute not in the relvar's heading (the target must already exist) |
| T0054 | no matching overload of an operator for the supplied argument types |
| T0055 | a `FormatText` (an `f"…"` literal or a `let` bound to one) is used outside a `template` argument (`format`'s or `write_line`'s) |
| T0056 | `format`'s `template` argument is neither an `f"…"` literal nor a `let` bound to one |
| T0057 | malformed placeholder in a format template (unmatched/empty/non-identifier `{…}`) |
| T0058 | format template references `{x}` but `args` has no attribute `x` |
| T0059 | _(warning)_ an `args` attribute is never used by the format template |
| T0060 | operator with this name + heading is already defined (a heading that exactly matches a built-in overload, a second user overload of the same name — only one is supported for now, pending linkage mangling — or redefining the `format` intrinsic). A user `oper` *may* extend a built-in name with a distinct heading. |
| T0061 | empty `Sequence []` has no element to infer from and no `let` type annotation to fall back on |
| T0062 | a `Sequence [ … ]` element's type differs from the first element's (sequences are homogeneous) |
| T0063 | a sequence literal appears outside a `let` binding value (the only position it is permitted) |
| T0064 | an **empty** `Sequence []` reached lowering with no element type to derive the payload layout from — a defensive fallback: an *annotated* empty binding (`let s: Sequence T = Sequence []`) now constructs from its annotation (like an empty `Relation {}`), and the unannotated / non-binding shapes are already rejected earlier (T0061 / T0063) (lowering) |
| T0065 | postfix index `s[i]` requires a `Sequence` operand (the operand has some other type) |
| T0066 | postfix index `s[i]` requires an `Integer` index (the index has some other type) |
| T0067 | `if` condition is not `Boolean` |
| T0068 | `if` arms have mismatched types — the `then` and `else` blocks must unify |
| T0069 | an `if` without `else` must have a Unit (`Tuple {}`) then-arm (the statement form) |
| T0070 | a UFCS method call `x.m {}` names an operator with no `self` parameter (not method-callable) |
| T0071 | a counted `for i := lo to hi` bound is not `Integer` (both bounds must be Integer) |
| T0072 | assignment to a `for` loop variable — it is loop-scoped and immutable |
| T0073 | `for … in` requires a `Sequence` operand; a `Relation` (or scalar) is rejected, pointing at `load … order` (the RM Pro 7 tuple-at-a-time boundary) |
| T0074 | reassignment (`x := …`) of an immutable `let` binding or a parameter — declare it with `var` to allow reassignment (a loop counter is the distinct T0072) |
| T0075 | reassignment (`x := …`) of a `var` whose RHS type differs from the binding's declared/inferred type |
| T0076 | reassigning a heap-managed `var` (`Sequence`/`Relation`/boxed `Tuple`, or a **flattened `Tuple` carrying heap cells**) *across a control-flow join* (a loop back-edge or `if` merge) is not yet lowered — value-typed and **owned `Text`** carries thread fine across both a loop and an `if`, and straight-line heap reassignment is fine (lowering) |
| T0077 | _(warning)_ a `var` is read but never reassigned — use `let` (a leading `_` opts out; the analog of Rust's `unused_mut`) |
| T0078 | an uninitialized `let x;` — an immutable binding must be initialized (use `var` for a later-assigned local) |
| T0079 | definite assignment: a `var` declared without a value (`var x;`) is read before it is assigned on all paths |
| T0080 | a `while` / `do … while` loop condition is not `Boolean` |
| T0081 | a `load … from <expr>` source is neither a `Relation` (forward form — order into a `Sequence`) nor a `Sequence` (reverse form — seal into a relvar); a scalar/`Tuple` source is rejected |
| T0082 | a `load … order [ … ]` sort key names a **relation- or tuple-valued** attribute — only scalars have an order (tuples/relations carry `=`/`<>` only, RM Pro 1) |
| T0083 | an `order` clause on the reverse `load <relvar> from <sequence>` form — a relation is unordered (RM Pro 1), so ordering a seal-into-relvar is meaningless |
| T0084 | the reverse `load` target is a **public** (SQL-backed) relvar — sealing a sequence into a public relvar (a DML replace) is not yet wired; use a private relvar |
| T0085 | a `type Name = …;` declaration shadows a built-in type name (`Integer`, `Text`, …) |
| T0086 | a `type Name …;` declaration (alias or possrep scalar) re-declares a name already given a type |
| T0087 | an operator that belongs to an opt-in stdlib module is called without importing it — add `use module <path>;` (e.g. `environment` needs `use module coddl::env;`) |
| T0088 | a type that belongs to an opt-in stdlib module is named without importing it — add `use module <path>;` (e.g. `RawRequest` needs `use module coddl::web;`) |
| T0089 | a `use module <path>;` names a module that does not exist under the reserved `coddl::` root |
| T0090 | a `builtin relvar` from an opt-in stdlib module is referenced without importing it — add `use module <path>;` (e.g. `Environment` needs `use module coddl::env;`) |
| T0091 | a possrep-scalar declaration `type Name { … }` has other than exactly one component — multi-component possreps are not yet supported (single-component tier) |
| T0092 | a name (an operator call, or a module-level `let` use) is exported by **more than one** imported userspace module (ambiguous import) — define it locally to disambiguate |
| T0093 | a `return` sits lexically inside a `transaction [...]` — its early exit would skip the transaction's commit, so it is rejected until real BEGIN/COMMIT lands (hoist the `return` out of the transaction, or restructure) |
| T0094 | `matching` / `not matching` operands have **identical** headings — the semijoin/antijoin matches on every attribute, which is a set intersection / difference; suggests `intersect` (matching) / `minus` (not matching) |
| T0095 | `matching` / `not matching` operands are **disjoint** (share no attribute) — a semijoin has no key to match on and degenerates to an existence guard on the left operand; rejected (like `join`/`compose`, the operands must partially overlap) |
| T0096 | a `Relation { … }` literal element is not a tuple — the relation selector's elements are tuple-typed expressions (a tuple literal `{a:1}`, or a tuple-valued name/call), and a relation is a set of tuples |
| T0097 | module-level `let` bindings form a reference cycle (bindings are order-independent; their initializers must form a DAG) |
| T0098 | module-level `let` initializer is missing or not a constant expression (calls, `transaction`, relvar reads, field access, `if`, and indexing are excluded until purity derivation / compile-time evaluation widen) |
| T0099 | `when` condition discipline: the condition must be `Boolean` (a relation-typed condition suggests `times`), and an unresolved name in it that is an attribute of the left operand hints at `where` — attributes are deliberately not in scope in a `when` condition |
| T0100 | `ungroup` target is not a relation-valued attribute (the RVA analogue of T0048) |
| T0101 | a storage-backed relvar (`public`/`base`) declares a relation- or tuple-valued attribute — no SQL column form yet; decompose into a side relvar (see [storage.md](storage.md) "Nested attributes") |
