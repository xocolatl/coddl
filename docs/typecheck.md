# Coddl typechecker

This document is the authoritative spec for what the typechecker
currently enforces: the `Type` representation, the built-in operator
registry, every walk function, and every `T####` diagnostic code.

For *why* the type system is shaped this way, see
`ARCHITECTURE.md §7 "Type system"` (possreps, selectors, `Tuple`/
`Relation`/`Sequence` generators, scalar built-ins, the `Tuple {}`
unit type). This document never duplicates that rationale — it points
at it and gets on with the rules.

**Last sync:** `9a67559`. Every commit that adds, removes, or changes a
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
| `Tuple H`     | Structural tuple with attribute set `H` (sorted by name). |
| `Unknown`     | Sentinel used during error recovery; equals anything.    |

`Tuple` with an empty attribute set (`Tuple {}`) is the **unit type** —
the implicit return of every `oper` declared without an explicit
return clause.

### Deliberately not yet typed

The following are deferred until the relevant productions arrive:

- **`Relation H`** and **`Sequence T`** — type generators referenced in
  `ARCHITECTURE.md §7` but not yet a `Type` enum variant. They land
  alongside `parse_type_ref` learning to parse generator applications.
- **User-defined scalar types** via `possrep` — the typechecker has
  no notion of user types yet; every type-name lookup either resolves
  to a built-in or yields `T0005`.
- **Function types** — `oper` signatures are stored in the built-in
  registry as `OperSig`, not as a first-class `Type` value.


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

- **`check_root`** — iterates the file's top-level items and dispatches
  each to `check_program_decl` or `check_oper_decl`.
- **`check_program_decl`** — no-op today; the program name is a label
  the runtime may use later. No semantic constraints yet.
- **`check_oper_decl`** — resolves the heading into a parameter scope
  (rejecting duplicate names with `T0007`), then checks the body
  against that scope. If the operator's name is `main`, its heading
  must be empty (`T0006`).
- **`resolve_type_name`** — maps a `TypeRef`'s identifier to a built-in
  `Type`. Unknown names produce `T0005` and resolve to `Type::Unknown`.
- **`check_block`** — walks statements with the surrounding parameter
  scope visible. Expression-statement results are discarded.
- **`check_expr_stmt`** — calls `check_expr` on the embedded expression
  and discards the result.
- **`check_expr`** — returns the expression's `Type`. Dispatches on
  the `Expr` variant:
  - `NameRef` looks up the name in the surrounding parameter scope.
    Unresolved names produce `T0001`.
  - `Literal` returns the type implied by the underlying token kind
    (`STRING_LIT` → `Text`, `CHAR_LIT` → `Character`, the three
    numeric kinds to the matching numeric type).
  - `Call` is `check_call`.
- **`check_call`** — the callee must be a `NameRef` whose lexeme is in
  the built-in registry (`T0001` otherwise). Each named argument
  is checked by `check_named_arg`. After the arguments, every
  declared parameter must have been supplied (`T0003`).
- **`check_named_arg`** — recognizes duplicate argument names
  (`T0008`), arguments not declared by the operator (`T0002`), and
  type mismatches between the argument's expression and the
  parameter's declared type (`T0004`). When the argument matches a
  declared parameter, it's marked provided.


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
