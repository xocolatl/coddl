# Coddl project plan

This document is the authoritative spec for the project-plan layer:
how the compiler walks from a `.cd` entry point to its companion
`.cddb` / `.cdstore` files, what it validates at every hand-off, and
the shape of the `Plan` data structure that downstream phases
(Phase 21 SQLite materialization, code generation) consume.

For *why* the layer exists, see `ARCHITECTURE.md §5 "Storage
abstraction"` (the `.cd` / `.cddb` / `.cdstore` separation, RM Pro 6
keeping physical layout out of D). This document never duplicates
that rationale — it points at it and gets on with the rules.

**Last sync:** Phase 16. Every commit that adds, removes, or changes a
PL-code or validation invariant in `crates/coddl-plan/` updates this
file in the same commit; `tools/check-grammar.sh` enforces the
diagnostic table from the hygiene gate.


## Discovery

`coddl_plan::discover_and_validate(cd_path) -> PlanOutput` is the
public entry point. It:

1. Reads the `.cd` source from `cd_path` and runs
   `coddl_types::check(_, _, FileKind::Cd)`. The per-file
   diagnostics flow into the output unchanged.
2. Reads the AST: extracts the `program <name>;` label (a no-op
   today; reserved for multi-program projects) and the
   `database <name>;` binding via
   `coddl_syntax::ast::DatabaseBinding::name()`.
3. **No public relvars in `.cd`** → returns an empty `Plan` with no
   PL diagnostics. The program builds standalone (Phase 8 path).
4. **Public relvars present, no binding** → PL0001 at the first
   public relvar's span.
5. **Public relvars present, binding present** → looks for
   `<db>.cddb` and `<db>.cdstore` in `cd_path`'s parent directory.
   Missing → PL0002 / PL0003.
6. Parses + typechecks each companion via `coddl_types::check`.
   `.cddb` populates a relvar table; `.cdstore` is consumed via the
   `ast_cdstore` AST view.

`.cdmap` discovery is **out of scope for Phase 16** — any `.cdmap`
file in the directory is left alone. Non-identity adapters (project /
rename / virtual sources) land in a later phase.


## Validation invariants (identity-only)

Cross-file validation runs only when the `.cd` declares at least one
public relvar. Each invariant emits a distinct PL-code anchored to
the offending span.

| Invariant                                                              | Diagnostic |
|------------------------------------------------------------------------|------------|
| `.cd` binds a database when public relvars are declared                | PL0001     |
| `<db>.cddb` exists in the same directory                               | PL0002     |
| `<db>.cdstore` exists in the same directory                            | PL0003     |
| `<db>.cddb`'s `database <name>;` equals the `.cd` binding              | PL0004     |
| `<db>.cdstore`'s `store for <name>;` equals the `.cd` binding          | PL0005     |
| Each public relvar in `.cd` has a same-named base relvar in `.cddb`    | PL0006     |
| Public-relvar heading equals catalog-relvar heading (set equality)     | PL0007     |
| Each resolved catalog relvar has exactly one `.cdstore` binding        | PL0008     |
| Each heading attribute appears in the `.cdstore` columns block         | PL0009     |
| `.cdstore` column entries name only attributes in the catalog heading  | PL0010     |
| The `backend <kind> { ... };` declaration is supported (v1: `sqlite`)  | PL0011     |

Heading equivalence reuses `coddl_types::Heading::assignable_to`, so
`Unknown` attribute types (the typechecker's error-recovery sentinel)
participate as wildcards. This avoids cascading a single upstream
T-code into a stack of PL-codes when the user has already been told
about the root cause.


## Plan data structure

```rust
pub struct Plan {
    pub program_name: String,
    pub database_name: Option<String>,    // None when no public relvars
    pub cd_relvars: RelvarTable,
    pub cddb_relvars: RelvarTable,        // empty when no companion loaded
    pub backend_kind: BackendKind,
    pub resolved: Vec<ResolvedPublicRelvar>,
}

pub struct ResolvedPublicRelvar {
    pub app_name: String,                 // declared in .cd
    pub catalog_name: String,             // == app_name for v1 identity
    pub heading: Heading,
    pub table_name: String,               // physical SQL table
    pub columns: Vec<(String, String)>,   // (heading_attr, sql_column)
    pub write_policy: WritePolicy,
}

pub enum WritePolicy {
    ReadOnly,                             // v1 SQLite forces this
    WriteThrough,                         // reserved for view-updating support
}

pub enum BackendKind {
    Sqlite,
    Other(String),                        // PL0011 records this
    Unknown,                              // companion absent or malformed
}
```


## Driver integration

`coddl plan <cd-file>` runs the plan pass and dumps the resolved
`Plan` to stdout (debug-style: program/database/backend, then one line
per resolved public relvar plus its columns). Diagnostics go to
stderr. Exit code: 0 on a clean check, 1 if any error severity fires.

`coddl check <cd-file>` extends to call the plan pass when:
- the input is a `.cd` file path (not stdin), AND
- the `.cd` typechecks to a relvar table with at least one `Public`
  entry.

Per-file typecheck diagnostics + plan diagnostics print together.
Standalone programs (no public relvars) stay on the single-file
path — no companion-file discovery, no PL diagnostics.


## PL-code table

Every diagnostic the plan layer emits has a stable `PL####` code.
Every code emitted in `crates/coddl-plan/src/` appears here; the
hygiene-check script enforces that.

| Code   | Trigger                                                                              |
|--------|--------------------------------------------------------------------------------------|
| PL0001 | `.cd` declares `public relvar`s but has no `database <name>;` binding                |
| PL0002 | `<db>.cddb` not found in the `.cd`'s parent directory                                |
| PL0003 | `<db>.cdstore` not found in the `.cd`'s parent directory                             |
| PL0004 | `<db>.cddb`'s `database <name>;` header doesn't match the `.cd`'s binding            |
| PL0005 | `<db>.cdstore`'s `store for <name>;` header doesn't match the `.cd`'s binding        |
| PL0006 | Public relvar has no matching base relvar in the catalog                             |
| PL0007 | Public-relvar heading doesn't match the catalog-relvar heading                       |
| PL0008 | Catalog relvar has no `.cdstore` binding                                             |
| PL0009 | `.cdstore` binding doesn't cover a heading attribute                                 |
| PL0010 | `.cdstore` column entry names an attribute not in the catalog heading                |
| PL0011 | Backend kind isn't supported (v1 supports `sqlite` only)                             |
| PL0100 | I/O error reading the `.cd` entry point                                              |
