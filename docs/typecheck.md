# Coddl typechecker

This document is the authoritative spec for what the typechecker
currently enforces: the `Type` representation, the built-in operator
registry, every walk function, and every `T####` diagnostic code.

For *why* the type system is shaped this way, see
`ARCHITECTURE.md §7 "Type system"` (possreps, selectors, `Tuple`/
`Relation`/`Sequence` generators, scalar built-ins, the `Tuple {}`
unit type). This document never duplicates that rationale — it points
at it and gets on with the rules.

**Last sync:** `1830ac1`. Every commit that adds, removes, or changes a
`T####` code, a built-in operator, or a typechecker walk method
updates this file in the same commit; `tools/check-grammar.sh`
enforces it from the hygiene gate.


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

- **`Sequence T`** — type generator referenced in
  `ARCHITECTURE.md §7` but not yet a `Type` enum variant. Lands with
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
  - `FieldAccess` is `check_field_access`.
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
