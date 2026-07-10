//! Lexer, parser, and concrete syntax tree for Coddl source.
//!
//! The parser produces a lossless concrete syntax tree: every token,
//! every comment, every byte of whitespace is preserved in the tree.
//! The typed AST consumed by the type checker and downstream passes is
//! a typed view derived from the CST, not a separate structure. One
//! tree, two views, no drift.
//!
//! The public surface — `Token`, `TokenKind`, `SyntaxKind`,
//! `SyntaxNode`, `lex`, and the AST view types — is plain data and pure
//! functions. No streams, no trait objects, no async at the API
//! boundary. Diagnostics are returned alongside output values, not
//! raised.

pub mod ast;
pub mod ast_cddb;
pub mod ast_cdmap;
pub mod ast_cdstore;
pub mod cst;
pub mod file_kind;
pub mod format_template;
pub mod lexer;
pub mod parser;
pub mod parser_cddb;
pub mod parser_cdmap;
pub mod parser_cdstore;
pub mod syntax_kind;
pub mod token;

pub use cst::{CoddlLanguage, CstBuilder, SyntaxElement, SyntaxNode, SyntaxToken};
pub use file_kind::FileKind;
pub use format_template::{parse_format_template, TemplateChunk, TemplateError, TemplateErrorKind};
pub use lexer::{lex, LexOutput};
pub use parser::parse;
pub use syntax_kind::SyntaxKind;
pub use token::{Token, TokenKind};

use coddl_diagnostics::Diagnostic;

/// A parsed source buffer: the syntax tree plus any diagnostics
/// collected while building it. The tree is always well-formed; nodes
/// for unrecoverable source ranges carry [`SyntaxKind::PARSE_ERROR`].
pub struct ParseOutput {
    pub tree: SyntaxNode,
    pub diagnostics: Vec<Diagnostic>,
}
