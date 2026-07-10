//! The `Plan` data structure produced by [`crate::discover_and_validate`].

use coddl_diagnostics::Diagnostic;
use coddl_types::{Heading, RelvarTable};

/// The output of one plan run: the synthesized [`Plan`] (when
/// discovery succeeded enough to shape it) and every diagnostic from
/// per-file typechecking and cross-file validation.
#[derive(Debug)]
pub struct PlanOutput {
    pub plan: Option<Plan>,
    pub diagnostics: Vec<Diagnostic>,
}

/// The kind declared by a `.cd` file's mandatory header — `program`,
/// `library`, or `module`. Read from the leading keyword of the file
/// header (`ProgramDecl::kind()`); classifies the compilation unit for
/// the driver (only a `program` is runnable) and the lowerer (only a
/// `program` splices lifecycle into `main`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileHeaderKind {
    /// `program p;` — an executable; requires an `oper main`.
    Program,
    /// `library l;` — a stable-C-ABI artifact a foreign host links; no `main`.
    Library,
    /// `module m;` — an intermediate unit linked into a consumer; no `main`.
    Module,
}

/// A program's resolved project plan — what the compiler knows about
/// the four-file `.cd` family for one entry point. Downstream phases
/// (Phase 21 SQLite materialization, code generation) consume this.
#[derive(Debug)]
pub struct Plan {
    /// The kind declared by the `.cd`'s file header. `None` only when the
    /// file has no header at all (a PL0012 error already fired); downstream
    /// callers bail on the diagnostic before relying on the kind.
    pub header_kind: Option<FileHeaderKind>,
    pub program_name: String,
    /// The database the program binds to via `database <name>;` in
    /// `.cd`. `None` when the program has no public relvars; that's
    /// the single-file standalone path.
    pub database_name: Option<String>,
    /// Relvars declared in the `.cd` (public + private).
    pub cd_relvars: RelvarTable,
    /// Relvars declared in the `.cddb` (base + virtual). Empty when no
    /// companion was loaded.
    pub cddb_relvars: RelvarTable,
    pub backend_kind: BackendKind,
    /// One entry per public relvar in `.cd` that resolved end-to-end
    /// through the chain.
    pub resolved: Vec<ResolvedPublicRelvar>,
    /// Default database file path baked into the binary at compile time.
    /// Comes from the `.cdstore`'s `backend sqlite { file: "..."; }`
    /// directive, canonicalized against the `.cdstore`'s parent directory
    /// when relative. `None` for non-SQLite backends, when the directive
    /// is absent, or when the program has no public relvars. The runtime
    /// resolver applies `CODDL_<DBNAME>_FILE` env override before falling
    /// back to this default.
    pub db_file_default: Option<String>,
}

/// One public relvar's full resolution: from the application-side
/// name through the catalog relvar to the physical SQL table.
#[derive(Debug, Clone)]
pub struct ResolvedPublicRelvar {
    /// The relvar's name as declared in `.cd`.
    pub app_name: String,
    /// The matching catalog relvar's name in `.cddb`. For v1 identity
    /// mappings this equals `app_name`; future non-identity adapters
    /// may rename.
    pub catalog_name: String,
    /// The canonical heading shared by the app and catalog relvars.
    pub heading: Heading,
    /// The physical SQL table name from `.cdstore`'s
    /// `relvar <Name>: table "<sql>" { … };`.
    pub table_name: String,
    /// Attribute-to-SQL-column mapping in heading-canonical (sorted)
    /// order. Each entry is `(heading_attr_name, sql_column_name)`.
    pub columns: Vec<(String, String)>,
    /// The catalog relvar's declared candidate keys — one inner `Vec` per
    /// key, each the key's attribute-name set. Carried so the SQL emitter
    /// can prove a read is already a set and elide `DISTINCT`.
    pub keys: Vec<Vec<String>>,
    /// What kinds of statements the runtime may issue against this
    /// relvar. A public relvar mapped to a **base** catalog relvar is
    /// directly writable (`ReadWrite`); one mapped to a **virtual**
    /// (view) relvar stays `ReadOnly` until `WriteThrough` view-updating
    /// semantics land.
    pub write_policy: WritePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePolicy {
    /// No writes — a view without write-through, or a non-writable backend.
    ReadOnly,
    /// Directly writable: the relvar maps 1:1 onto a base SQL table, so
    /// surgical DML (`delete`/`insert`/`update`, relational assignment)
    /// goes straight to that table.
    ReadWrite,
    /// Writable via view-updating translation. Reserved — not yet emitted.
    WriteThrough,
}

/// The classification of the `.cdstore`'s `backend <kind> { ... };`
/// declaration. v1 supports `Sqlite` only; other declarations are
/// recorded so Phase 21 can produce a helpful runtime error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    Sqlite,
    Other(String),
    /// Companion `.cdstore` absent or its `backend` declaration is
    /// malformed beyond recognition.
    Unknown,
}
