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
//! Reason: the formatter (`coddl-fmt`) needs every byte; the LSP wants
//! incremental re-parse later (under `salsa`); a side-channel trivia stream
//! makes "where does this comment attach?" a re-decision every formatter
//! pass. One lossless tree, two views, no drift.
//!
//! Every CST node carries a [`coddl_diagnostics::Span`] (§12 discipline #1).

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
