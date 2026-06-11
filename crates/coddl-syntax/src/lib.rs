//! Lexer, parser, and concrete syntax tree (CST) for Coddl source.
//!
//! Surface syntax: uniform named-argument prefix style (ARCHITECTURE.md §3).
//! The parser uses `chumsky` with error recovery enabled — never bail on
//! the first syntax error; produce a tree with `Error` nodes where things
//! broke (§12 discipline #2).
//!
//! ## CST, not a plain AST (§13)
//!
//! The parser produces a **lossless concrete syntax tree** — every token,
//! every comment, every byte of whitespace preserved. The typed AST that
//! the type checker and downstream passes consume is a *view* derived
//! from the CST, not a separate structure.
//!
//! Every CST node carries a [`coddl_diagnostics::Span`] (§12 discipline #1).
//!
//! ## Coddl-rewritability
//!
//! The public surface of this crate is shaped so a future Coddl
//! self-host rewrite mirrors it 1:1. Concretely:
//!
//! - `Token` and `TokenKind` are plain data — a `#[repr(C)]` record and
//!   a flat enum. Both translate directly to a Coddl `Tuple` and a Coddl
//!   sum type once those land.
//! - `lex(source, file) -> LexOutput` is a pure function; no state, no
//!   trait objects, no async.
//! - Diagnostics are values returned alongside the output (§12 discipline #3).
//!
//! Internally the lexer uses `chumsky` because it does the job well; the
//! consumer-facing data doesn't care.

pub mod token;
pub mod lexer;

pub use token::{Token, TokenKind};
pub use lexer::{lex, LexOutput};

use coddl_diagnostics::{Diagnostic, Span};

/// Result of parsing a source buffer: a (possibly partial) tree plus any
/// diagnostics produced along the way.
pub struct ParseOutput<T> {
    pub tree: T,
    pub diagnostics: Vec<Diagnostic>,
}

/// Placeholder for the top-level AST view — a Coddl program.
///
/// Will grow into the real AST in milestone 1 step 1, derived from the CST.
#[derive(Debug, Default)]
pub struct Program {
    pub span: Span,
    // TODO: items (oper decls, scalar type decls, relvar decls, constraint decls, …)
}
