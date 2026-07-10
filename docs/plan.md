# Plan layer — `.cd` / `.cddb` / `.cdmap` / `.cdstore`

The plan layer is the bridge between Coddl source (which never names a file path, a connection string, or a dialect — RM Pro 6) and the physical storage the runtime opens. This doc covers the four-file separation, the `database <name>;` binding, and the spec for `coddl-plan`: how the compiler walks from a `.cd` entry point to its companions, what it validates at every hand-off, and the shape of the `Plan` data structure that downstream code generation and runtime materialization consume.

## The four-file separation (RM Pro 6)

TTM's RM Pro 6 forbids internal-level constructs in `D` source. A Coddl program does not name a `.sqlite` file, a `pg_hba.conf`, a `JDBC URL`, or a column type. It names a **database** (a logical handle) by binding `database <name>;`; everything operational lives in companion files keyed by that name:

- **`.cd`** — the program. Public relvar declarations name the *external schema* (the program's view onto the catalog). Bare references to public relvars read like ordinary variables. No file paths anywhere.
- **`<db>.cddb`** — the database catalog. Conceptual schema: `base relvar`s, `virtual relvar`s (views), constraints. Multiple `.cd` programs can target the same `.cddb`.
- **`<db>.cdstore`** — the conceptual-to-physical binding. Backend kind (`sqlite` for v1; see [risks.md](risks.md)), file path (or DSN), per-table column-to-attribute mapping. Operational fields (file paths, credentials) can be overridden by environment variables at runtime — see [storage.md](storage.md) "Path resolution."
- **`<db>.cdmap`** *(planned, not v1)* — non-identity adapters: project / rename / virtual-relvar sources where the external `.cd` view differs from the catalog `.cddb` shape. v1 supports identity-only mapping; `.cdmap` lands in a later phase.

The separation is one of the long-term-planning bills paid up front (see [principles.md](principles.md)). The same `.cd` program could be re-bound to a different `.cdstore` (SQLite for dev, Postgres for prod) without touching source. The `.cddb` and `.cdstore` separation lets the operational config evolve independently of the conceptual schema.

## The `database <name>;` declaration

Every Coddl `.cd` source with public relvars must declare which database it binds to:

```
program hello_world_db;
database greetings;

public relvar Greetings {
    id: Integer,
    message: Text,
}
key { id };
```

`database greetings;` introduces the logical handle. The plan layer then expects `greetings.cddb` and `greetings.cdstore` to exist in the same directory as the `.cd` file. The names line up by convention: file basename equals binding name.

Runtime operational overrides key off the binding name uppercased: `CODDL_GREETINGS_FILE` overrides the SQLite path for the `greetings` database in this example. See [storage.md](storage.md) for the full operational-field resolver.

A `.cd` source with **no** public relvars doesn't need a `database` binding — it's a standalone program. The plan layer detects this and returns an empty `Plan`; downstream phases handle the no-database case (no slot initialization, no transaction externs to wire).

---

# Implementation spec

The rest of this doc pins what `coddl-plan` enforces today.

**Last sync:** file-kind headers (PL0012–PL0015). Every commit that adds, removes, or changes a PL-code or validation invariant in `crates/coddl-plan/` updates this file in the same commit; `tools/check-grammar.sh` enforces the diagnostic table from the hygiene gate.


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
2a. **Validates the mandatory file header** (`validate_file_header`),
   unconditionally, before any public-relvar branching: exactly one
   `program`/`library`/`module` header as the first item (PL0012 /
   PL0013), and the kind⟺`main` rule (`program` requires an `oper main`
   → PL0014; `library`/`module` forbid one → PL0015). The resolved
   `FileHeaderKind` is threaded into the `Plan` so the driver can gate
   commands (only a `program` is runnable) and the lowerer can choose
   lifecycle emission. These are *compilation-unit* rules, kept out of
   `coddl_types::check` so that reusable frontend stays lenient for the
   LSP's partial buffers and unit-test fragments.
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
    pub header_kind: Option<FileHeaderKind>,  // None only when no header at all
    pub program_name: String,
    pub database_name: Option<String>,    // None when no public relvars
    pub cd_relvars: RelvarTable,
    pub cddb_relvars: RelvarTable,        // empty when no companion loaded
    pub backend_kind: BackendKind,
    pub resolved: Vec<ResolvedPublicRelvar>,
}

pub enum FileHeaderKind { Program, Library, Module }

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
| PL0012 | `.cd` file has no `program`/`library`/`module` header, or it is not the first item    |
| PL0013 | `.cd` file declares more than one file header                                        |
| PL0014 | A `program` declares no `oper main` entry point                                      |
| PL0015 | A `library`/`module` declares an `oper main` (only a `program` has an entry point)   |
| PL0100 | I/O error reading the `.cd` entry point                                              |
