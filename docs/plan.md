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

**Last sync:** catalog-rooted resolution (`resolve_catalog` / `CatalogPlan`, PL0020) for `coddl provision`. Every commit that adds, removes, or changes a PL-code or validation invariant in `crates/coddl-plan/` updates this file in the same commit; `tools/check-grammar.sh` enforces the diagnostic table from the hygiene gate.


## Discovery

`coddl_plan::discover_and_validate(cd_path) -> PlanOutput` is the
public entry point. It:

1. Parses the `.cd`, resolves its userspace module graph (see
   "Userspace module resolution" below), then runs the **multi-unit**
   `coddl_types::check_program` over the entry plus every resolved
   module (dependency-first), so cross-module calls resolve and each
   module body is checked. The merged, per-`FileId`-tagged diagnostics
   flow into the output; the entry unit's `CheckOutput` drives the rest
   of discovery. FileIds: entry = `0`, `.cddb`/`.cdstore` companions
   reserve `1`/`2`, modules take `3..`.
2. Reads the entry AST: extracts the `program <name>;` label (a no-op
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
   `<db>.cddb` in `cd_path`'s parent directory. Missing → PL0002.
6. Parses + typechecks the `.cddb` via `coddl_types::check`, which
   populates a relvar table. Physical binding is identity (table =
   relvar name, column = attribute); the plan layer does not read a
   `.cdstore` (that is the future storage-catalog loader's job).

`.cdmap` discovery is **out of scope for Phase 16** — any `.cdmap`
file in the directory is left alone. Non-identity adapters (project /
rename / virtual sources) land in a later phase.


## Userspace module resolution

After validating the header, `discover_and_validate` resolves the
entry file's `use module <path>;` imports (`crates/coddl-plan/src/modules.rs`).
It walks the imports transitively, building a `ModuleGraph` — the set of
userspace modules reachable from the entry point, in **dependency-first**
order (a module appears after every module it imports) — attached to
`PlanOutput.module_graph`. A later phase type-checks and lowers these units.

Resolution runs for **every** entry file, independent of public relvars, so a
standalone `program` that imports a module is validated too.

- **Roots and providers.** A module path is resolved by a
  `coddl_stdlib::ModuleProvider`. The reserved `coddl` first segment is the
  embedded stdlib (`coddl_stdlib::EmbeddedProvider`), handled by the
  typechecker; the plan layer skips it. Any other path is a **userspace**
  module resolved by the project-local `modules::FsProvider`, which maps a
  single-segment leaf `foo` to a sibling `foo.cd` under the importing file's
  directory — the same by-convention resolution as `database greetings;` →
  `greetings.cddb` (a module path is a *logical name*, never a filesystem path
  in source). The provider consults the plan layer's `overrides` map (unsaved
  LSP buffers) before touching disk.

- **Validation** (each a zero-span diagnostic whose message names the importing
  file, the module, and the expected path — precise per-file spans arrive with
  the multi-file source map):
  - **PL0016** — the sibling `<leaf>.cd` is unreadable, or the path is nested
    (multi-segment userspace paths are not yet supported).
  - **PL0017** — the target's header declares a name that isn't the file's leaf,
    *case-exactly*. This is the guard against case-folding filesystems
    (macOS/Windows), where `foo.cd` and `Foo.cd` are the same file: the
    self-declared header is source-of-truth, so a mismatch is a compile error
    rather than a silent mis-resolution.
  - **PL0018** — the target is a `program`/`library` (or headerless), not a
    `module`. `use module` links `module` units only.
  - **PL0019** — an import cycle among modules (detected as a back-edge in the
    DFS walk).


## Validation invariants (identity-only)

Cross-file validation runs only when the `.cd` declares at least one
public relvar. Each invariant emits a distinct PL-code anchored to
the offending span.

| Invariant                                                              | Diagnostic |
|------------------------------------------------------------------------|------------|
| `.cd` binds a database when public relvars are declared                | PL0001     |
| `<db>.cddb` exists in the same directory                               | PL0002     |
| `<db>.cddb`'s `database <name>;` equals the `.cd` binding              | PL0004     |
| Each public relvar in `.cd` has a same-named base relvar in `.cddb`    | PL0006     |
| Public-relvar heading equals catalog-relvar heading (set equality)     | PL0007     |

The physical binding is **identity** — table = relvar name, column =
attribute (the mapping `coddl::storage`'s design mandates) — and the backend +
connection file are transitional defaults (SQLite, `<db>.sqlite`). The plan
layer no longer reads a `.cdstore`; a `.cdstore` is now DML into `coddl::storage`
(see `docs/cdstore-grammar.md`) that the future storage-catalog **loader** will
consume. Until it lands, PL0003 / PL0005 / PL0008 / PL0009 / PL0010 / PL0011
(the retired `.cdstore`-binding invariants) are not emitted.

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


## Catalog-rooted resolution (`resolve_catalog`)

`coddl provision` seeds a database from its catalog, so it resolves
**from a `.cddb`**, not from a `.cd` program.
`coddl_plan::resolve_catalog(cddb_path) -> CatalogPlanOutput` is the
parallel entry point (`crates/coddl-plan/src/catalog.rs`). It reads +
typechecks the `.cddb` and resolves every **base** relvar to its
physical form by **identity**: table = relvar name, column = attribute.
The backend + connection file are transitional defaults (SQLite,
`<db>.sqlite`) — TODO(cdstore-loader): resolve them by querying the
loaded `coddl::storage` relations. The `.cddb` is `FileId(0)`.

```rust
pub struct CatalogPlanOutput {
    pub plan: Option<CatalogPlan>,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct CatalogPlan {
    pub database_name: String,               // from `.cddb` `database <name>;`
    pub backend_kind: BackendKind,           // default: SQLite (TODO loader)
    pub db_file_default: Option<String>,     // default: `<db>.sqlite` (TODO loader)
    pub relvars: Vec<ResolvedCatalogRelvar>, // base only, name-sorted
}

pub struct ResolvedCatalogRelvar {
    pub name: String,
    pub heading: Heading,
    pub keys: Vec<Vec<String>>,
    pub table_name: String,                    // identity: = name
    pub columns: Vec<(String, String)>,        // identity: (attr, attr), name-sorted
    pub init: Option<coddl_syntax::ast::Expr>, // RHS of `Name := <expr>;`; the seed value
}
```

`init` carries the INIT relation **expression** node (the RHS of
`<Name> := <expr>;`), not folded rows: an INIT cell is any constant
expression, so `coddl-provision` — not the plan layer — evaluates it
to seed rows, above the neutral backend seam. That node is an
`Rc`-backed rowan handle, so `CatalogPlan` is `!Send` (provision is a
synchronous CLI pass; the program flow's `Plan` is a separate `Send`
type).

Env-var resolution (`CODDL_<DBNAME>_FILE`) is **not** applied here:
`resolve_catalog` carries `database_name` + `db_file_default`, and
`coddl-provision` applies the override to replicate the runtime
resolver, so `provision` and `run` open the same file.

Behavior:
- **Hard failures** (plan `None`): the `.cddb` is unreadable (PL0100),
  or it has no `database <name>;` header (PL0020).
- Otherwise a plan is returned; every base relvar resolves by identity.
  Virtual relvars are omitted (no physical table).

Only PL0100 / PL0020 apply to the catalog flow; the `.cddb`'s own
typecheck diagnostics (T-codes) flow through the merged output. The
retired `.cdstore`-binding codes (PL0003 / PL0005 / PL0008–PL0011) are
no longer emitted by either flow.


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
| PL0016 | A `use module <leaf>;` import resolves to no readable module file (or a nested userspace path) |
| PL0017 | An imported module's header name doesn't match its file name exactly (case-fold guard) |
| PL0018 | A `use module` import targets a `program`/`library` (or a headerless file), not a `module` |
| PL0019 | Import cycle among userspace modules                                                 |
| PL0020 | A `.cddb` catalog has no `database <name>;` header, so its `.cdstore` can't be located |
| PL0100 | I/O error reading the entry point (`.cd` program or `.cddb` catalog)                 |
