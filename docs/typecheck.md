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

**Static operator overloading is permitted.** A few comparison operators resolve to distinct underlying operators depending on the operand type family — most notably, `<=` and `>=` are scalar comparison on scalars and **subset** / **superset** on relations (`<` and `>` give strict subset / superset). The same identifier names two operators; the type checker picks which based on operand types at compile time. RM Pre 8 monomorphism is preserved because each underlying operator is type-monomorphic; the surface `<=` is just a shared spelling, the same way `+` can be spelled by `Integer` addition and `Rational` addition without violating RM Pre 8.

### No nulls

The type system has no nullable-attribute facility. Missing information is a database-design problem the user solves through **vertical decomposition** — splitting the relvar so the absence of a fact is the absence of a tuple in a side relvar (the canonical TTM answer; ch. 7, RM Pro 4). A user-defined sum-type scalar (`Optional` with `Some` / `None` possreps) is permitted by the type system but not the recommended approach. The SQL backend never sees a request to emit a NULL — see [sqlemit.md](sqlemit.md).

## Type generators

- `Tuple { a: T, b: U, … }` and `Relation { a: T, b: U, … }` are type generators producing structurally-identified types: `Tuple H1 = Tuple H2` iff `H1 = H2` as sets of `{name: type}` pairs. Same for `Relation`. Attribute order is immaterial. Both generators may take zero attributes (`reltrue` and `relfalse` are the only inhabitants of `Relation {}` — see the naming note below).
- **`Tuple {}` is the unit type** — the type of a tuple with no attributes. It has exactly one value, written `{}` (the empty tuple literal). This is Coddl's analogue of Rust's `()`, Swift's `Void`, or the unit type in ML. An `oper` declared without an explicit return clause implicitly returns `Tuple {}`. The two spellings `Tuple {}` and the value `{}` are unambiguous in context — one appears in type position, the other in expression position.
- `Sequence T` is the ordered counterpart — a finite ordered list of values of element type `T`, duplicates allowed, position significant. It's the procedural-side companion to `Relation`: where `Relation H` is an unordered set of tuples (RM Pro 1, 3), `Sequence Tuple H` is an ordered list of tuples (the canonical iteration form — see [runtime.md](runtime.md) "load"). The element type `T` may be any type — primitives (`Sequence Integer`), tuples (`Sequence Tuple H` — the typical case), or even relations (`Sequence Relation H`).
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

## Relations are fully first-class

Relations can be bound to variables, passed to and returned from operators, stored in tuples, nested inside other relations, used as function arguments and results everywhere a scalar can — subject to the lazy-evaluation semantics in [runtime.md](runtime.md). The calling convention treats them uniformly.

## Type inference and constraint inference

Type inference for relational expressions is mandatory and mechanical from operator semantics (RM Pre 18): every RelIR node's heading is the heading of its operands transformed by its operator. The optimizer further runs:

- **FD propagation** for candidate-key inference (VSS 3) — best-effort.
- **Constraint propagation** (RM Pre 23): predicates known to hold on operands propagate through restrict, project, join, extend, etc. Used for view-constraint checking and as optimizer hints.

## Where constraints can live

Integrity constraints attach only to **database relvars** (real, virtual). Coddl does not support constraints on application relvars (private or public), tuple variables, or scalar variables — there's "no logical reason why it should not," as TTM acknowledges (ch. 5 p. 106), but the cost in implementation complexity outweighs the payoff for the use cases we've identified so far. Revisit if a concrete need surfaces.

---

# Implementation spec

The rest of this doc pins what `coddl-types` enforces today.

**Last sync:** `1830ac1`. Every commit that adds, removes, or changes a `T####` code, a built-in operator, or a typechecker walk method updates this file in the same commit; `tools/check-grammar.sh` enforces it from the hygiene gate.


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

- **`Sequence T`** — type generator described in the "Type generators"
  section above but not yet a `Type` enum variant. Lands with
  the `LOAD ARRAY ... ORDER (...)` iteration form.
- **User-defined scalar types** via `possrep` — the typechecker has
  no notion of user types yet; every type-name lookup either resolves
  to a built-in or yields `T0005`.
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

The `Builtins` table maps operator names to their `OperSig`. A call
whose callee is a `NameRef` looks up its lexeme in this table; an
unknown name produces `T0001`.

| Name         | Heading                | Returns    |
|--------------|------------------------|------------|
| `write_line` | `{ message: Text }`    | `Tuple {}` |

More operators arrive as the runtime grows.


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
  outer ones.
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
  `check_tuple_lit`. Empty `Relation {}` emits `T0018` (no
  inference context for the heading) and returns `Type::Unknown`.
  The first tuple's heading establishes the relation's heading;
  every subsequent tuple must match (per `Heading::assignable_to`);
  mismatches emit `T0019` on the offending tuple without cascading
  the failure. Returns `Type::Relation(h0)`.
- **`write_relation` polymorphism** — the built-in's `rel`
  parameter has `ParamKind::AnyRelation` rather than a concrete
  type. `check_named_arg` special-cases this kind: any
  `Type::Relation(_)` matches regardless of heading; non-relation
  args emit `T0004`. The per-call-site heading is carried through
  the lowerer's `value_types` map and into the backend via
  `Inst::WriteRelation`'s `heading_id` field.
- **`check_binary_expr`** — dispatches on the parsed `BinaryOp`:
  - **Comparison (`=`, `<>`)**: operands must share a scalar type
    (Integer, Text, or Boolean for v1). Result is `Boolean`. T0021 on
    mismatch.
  - **Ordering (`<`, `>`, `<=`, `>=`)**: both operands must be
    Integer. Result is `Boolean`. T0021 otherwise.
  - **Logical (`and`, `or`)**: both operands must be Boolean.
    Result is `Boolean`. T0021 otherwise.
  - **`where`**: lhs must be `Relation H` (T0023 if not). A fresh
    scope layer is pushed with the heading's attributes as
    bindings, then the rhs (predicate) is checked; the predicate
    must be `Boolean` (T0020 otherwise). The scope is popped after
    the predicate. Result is `Relation H` (lhs's type unchanged).
- **Capture deferral (T0022)** — Phase 20 deferred capture support
  for `where` predicates. The typechecker's scope lookup walks
  innermost-first so an outer let binding would technically
  resolve; the **lowerer** detects this case (NameRef misses in
  the predicate's heading-scope but hits the saved enclosing
  scope) and emits T0022. Future phase will lift this restriction
  via a user_data pointer threaded through `coddl_relation_where`.
- **`check_unary_expr`** — dispatches on the parsed `UnaryOp`.
  Phase 21 ships `Extract` only. The operand must be
  `Type::Relation(H)`; the result is `Type::Tuple(H)`. T0024 on
  a non-relation operand. Other unary ops slot in here.
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
- **`check_rename_expr`** — `R rename { old: new, … }`. The operand must be
  `Type::Relation(H)` (T0023). Each target must be a bare attribute name
  (T0030); each source must exist in `H` (T0029); the rename must stay a
  bijection — no source repeats and no target collides with a surviving
  attribute (T0031). The result is `Type::Relation(H')` with names remapped,
  canonically re-sorted. A relvar-rooted rename pushes to SQL (each renamed
  attribute aliased `AS` its new name); an in-memory operand lowers to the
  in-process `Inst::Rename` → `coddl_relation_rename`, which permutes record
  bytes into the re-sorted layout and re-seals.


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
- Existing side-effecting builtins: `write_line`, `write_relation`.
  All future builtins default to `Pure` in the registry and must
  opt in to `SideEffecting` explicitly — adding a printing operator
  is a forcing function on the conformance check.
- `check_call`: when `transaction_depth > 0` and the resolved callee
  is `SideEffecting`, emit `T0026` at the callee token.
- The rule applies recursively through nested `transaction [...]`
  blocks (they only ever increase depth) and will extend to user-
  defined `oper`s once those callees carry a derived purity flag.


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
| T0018 | Empty relation literal — no inference context for heading |
| T0019 | Tuple heading mismatch in relation literal                |
| T0020 | `where` predicate must be Boolean                         |
| T0021 | Scalar operator operand type mismatch                     |
| T0022 | Captured identifier in `where` predicate not yet supported |
| T0023 | `where` / `project` / `rename` left operand is not a relation |
| T0024 | `extract` operand is not a relation                       |
| T0025 | Public relvar referenced outside any `transaction [...]`  |
| T0026 | Side-effecting operator called inside `transaction [...]` |
| T0027 | Unknown attribute name in a `project` list                |
| T0028 | Duplicate attribute name in a `project` list              |
| T0029 | Unknown attribute name in a `rename` source               |
| T0030 | `rename` target must be a bare attribute name             |
| T0031 | `rename` is not a bijection (duplicate source or target collision) |
