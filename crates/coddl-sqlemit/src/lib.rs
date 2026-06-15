//! RelIR → SQL emission and the storage-backend traits.
//!
//! The traits in this crate are implemented by `coddl-backend-sqlite`
//! and `coddl-backend-postgres`. SQL emission follows the mandatory
//! rules table in `docs/sqlemit.md` — `SELECT DISTINCT` everywhere,
//! never `NULL`/`NULLABLE`/`IS NULL`/outer joins, explicit `BEGIN`/`COMMIT`,
//! enumerate columns in deterministic order, etc.
//!
//! This crate is shared between the compiler and the runtime — the SQL
//! emitter must be callable from a `staticlib` linked into user binaries
//! (for plans built at runtime; see `docs/runtime.md` "Reaching the engines").

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    SQLite,
    Postgres,
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dialect::SQLite => f.write_str("sqlite"),
            Dialect::Postgres => f.write_str("postgres"),
        }
    }
}

/// A piece of SQL with its parameter slots already enumerated.
///
/// Parameters are positional in the emitted text (`?` for SQLite,
/// `$1`, `$2` for Postgres) but the higher-level compiler tracks them
/// by name; the binding step (Conn::bind_and_step) takes them in
/// declaration order.
#[derive(Clone, Debug)]
pub struct SqlString {
    pub text: String,
    pub param_count: u32,
}

/// Identifier for a prepared statement cached by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StmtId(pub u32);

/// Identifier for a backend-side temp relation that an in-memory relation
/// was shipped into (see `docs/sqlemit.md` "Sending in-memory relations
/// back into SQL").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TempRelRef(pub u32);

/// Opaque shapes used by the `Backend` and `Conn` trait signatures.
/// The real definitions live in `coddl-relir` and `coddl-types`; the
/// emitter only sees them as values to render.
pub struct RelPlan;
pub struct Schema;
pub struct TypeMap;
pub struct Heading;
pub struct Tuple;
pub struct Value;
pub struct Dsn;
pub struct RowIter<'a> {
    _phantom: std::marker::PhantomData<&'a ()>,
}

#[derive(Debug)]
pub enum BackendError {
    Connect(String),
    Prepare(String),
    Step(String),
    Other(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Connect(m) => write!(f, "connect: {m}"),
            BackendError::Prepare(m) => write!(f, "prepare: {m}"),
            BackendError::Step(m) => write!(f, "step: {m}"),
            BackendError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for BackendError {}

pub type Result<T> = std::result::Result<T, BackendError>;

/// Backend = SQL-emission half (pure) + factory for connections.
///
/// The compiler picks one backend at build time via a Cargo feature on
/// the runtime crate. If passing backends around as values gets clumsy
/// with the associated-type form, the planned escape hatch (§5) is a
/// `dyn`-friendly `BackendOps` record-of-fn-pointers; decide once the
/// second backend exists.
pub trait Backend {
    type Conn: Conn;

    fn dialect(&self) -> Dialect;
    fn emit_select(&self, plan: &RelPlan) -> SqlString;
    fn emit_ddl(&self, schema: &Schema) -> Vec<SqlString>;
    fn type_map(&self) -> &TypeMap;
    fn open(&self, dsn: &Dsn) -> Result<Self::Conn>;
}

/// Conn = effectful side: own a live connection, prepare/cache statements,
/// step rows, ship in-memory relations to temp tables.
pub trait Conn {
    fn prepare(&mut self, sql: &SqlString) -> Result<StmtId>;
    fn bind_and_step<'a>(&'a mut self, id: StmtId, params: &[Value]) -> Result<RowIter<'a>>;
    fn materialize_temp(&mut self, heading: &Heading, rows: &[Tuple]) -> Result<TempRelRef>;
}
