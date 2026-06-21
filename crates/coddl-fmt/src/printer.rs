//! Token-stream pretty-printer over the lossless CST.
//!
//! The formatter is deliberately a *re-spacer*, not a re-wrapper: it walks the
//! flat sequence of leaf tokens, drops the `WHITESPACE` trivia, and recomputes
//! the separator (space / newline+indent / blank line) between every adjacent
//! pair from a small set of token-adjacency rules plus bracket-nesting context.
//! Comments are ordinary tokens kept in place, so they survive verbatim. Line
//! breaks inside a `{…}` / `[…]` / `(…)` group are preserved (a group that
//! spanned multiple source lines stays multi-line; a single-line one stays
//! inline) — width-based wrapping is deferred.

use coddl_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use SyntaxKind::*;

/// Render `root` to canonical source with `indent_width`-space indentation.
pub fn print(root: &SyntaxNode, indent_width: usize) -> String {
    let toks: Vec<SyntaxToken> = root
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .collect();

    let multiline = compute_multiline(&toks);

    let mut out = String::new();
    let mut indent: usize = 0;
    // Stack of `multiline` flags for the currently-open bracket groups, so the
    // comma rule knows whether its enclosing group breaks one-per-line.
    let mut groups: Vec<bool> = Vec::new();
    // The previous *significant* (non-whitespace) token index, and whether the
    // whitespace gap since it contained a newline / a blank line.
    let mut prev: Option<usize> = None;
    let mut gap_newline = false;
    let mut gap_blank = false;

    for (i, tok) in toks.iter().enumerate() {
        let k = tok.kind();
        if k == WHITESPACE {
            let nls = tok.text().matches('\n').count();
            if nls >= 1 {
                gap_newline = true;
            }
            if nls >= 2 {
                gap_blank = true;
            }
            continue;
        }

        // Dedent before a multi-line closing bracket.
        if is_close(k) && multiline[i] {
            indent = indent.saturating_sub(1);
        }

        if let Some(p) = prev {
            match separator(&toks, p, i, &multiline, &groups, gap_newline, gap_blank) {
                Sep::None => {}
                Sep::Space => out.push(' '),
                Sep::Newline => push_newline(&mut out, indent, indent_width, false),
                Sep::Blank => push_newline(&mut out, indent, indent_width, true),
            }
        }

        push_token(&mut out, tok);

        // Maintain bracket context.
        if is_open(k) {
            groups.push(multiline[i]);
            if multiline[i] {
                indent += 1;
            }
        } else if is_close(k) {
            groups.pop();
        }

        prev = Some(i);
        gap_newline = false;
        gap_blank = false;
    }

    // Exactly one trailing newline; no trailing blank lines or whitespace.
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

#[derive(Clone, Copy)]
enum Sep {
    None,
    Space,
    Newline,
    Blank,
}

/// Decide the separator emitted *before* token `i`, given the previous
/// significant token `p`.
fn separator(
    toks: &[SyntaxToken],
    p: usize,
    i: usize,
    multiline: &[bool],
    groups: &[bool],
    gap_newline: bool,
    gap_blank: bool,
) -> Sep {
    let pk = toks[p].kind();
    let ck = toks[i].kind();

    // A line comment ends its line, so whatever follows starts a new one.
    if pk == LINE_COMMENT {
        return if gap_blank { Sep::Blank } else { Sep::Newline };
    }

    // A comment keeps its source placement: trailing (same line as preceding
    // code) stays trailing after one space; a comment on its own line in the
    // source stays on its own line.
    if is_comment(ck) {
        return if gap_newline {
            if gap_blank {
                Sep::Blank
            } else {
                Sep::Newline
            }
        } else {
            Sep::Space
        };
    }

    // Multi-line bracket structure.
    if is_close(ck) && multiline[i] {
        return Sep::Newline;
    }
    if is_open(pk) && multiline[p] {
        return Sep::Newline;
    }

    // Statement / declaration / member boundary: one per line at top level and
    // inside a multi-line group (block); an inline group stays on one line.
    if pk == SEMICOLON {
        if groups.last().copied().unwrap_or(true) {
            return if gap_blank { Sep::Blank } else { Sep::Newline };
        }
        return Sep::Space;
    }
    // One field/argument per line inside a multi-line group.
    if pk == COMMA {
        return if groups.last().copied().unwrap_or(false) {
            Sep::Newline
        } else {
            Sep::Space
        };
    }

    // No space before these.
    if matches!(ck, COMMA | SEMICOLON | COLON | DOT | R_PAREN | R_BRACKET) {
        return Sep::None;
    }
    // No space after these.
    if matches!(pk, DOT | L_PAREN | L_BRACKET) {
        return Sep::None;
    }
    // Empty braces stay tight: `{}`.
    if pk == L_BRACE && ck == R_BRACE {
        return Sep::None;
    }
    // Inline brace padding: `{ a: 1 }`.
    if pk == L_BRACE || ck == R_BRACE {
        return Sep::Space;
    }
    // Space after `:` (none before is handled above).
    if pk == COLON {
        return Sep::Space;
    }
    // Spaces around infix/assignment/arrow operators.
    if is_op(pk) || is_op(ck) {
        return Sep::Space;
    }

    // Default: a single space (covers `program foo`, `oper main`, `r join s`,
    // `} key`, identifier/keyword adjacency, etc.).
    Sep::Space
}

struct Group {
    open: usize,
    /// A newline appeared inside this group's source.
    saw_newline: bool,
    /// A non-bracket token appeared inside (the group isn't empty).
    has_content: bool,
    /// The `[` of a statement `BLOCK` (oper / transaction body) — these are
    /// always laid out one-statement-per-line, even if the source was inline.
    is_block: bool,
}

/// For each bracket token, whether its group should be laid out multi-line:
/// it spanned multiple source lines, OR it is a non-empty statement block.
fn compute_multiline(toks: &[SyntaxToken]) -> Vec<bool> {
    let mut multiline = vec![false; toks.len()];
    let mut stack: Vec<Group> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let k = tok.kind();
        if k == WHITESPACE {
            if tok.text().contains('\n') {
                if let Some(top) = stack.last_mut() {
                    top.saw_newline = true;
                }
            }
            continue;
        }
        if !is_open(k) && !is_close(k) {
            if let Some(top) = stack.last_mut() {
                top.has_content = true;
            }
        }
        if is_open(k) {
            let is_block = k == L_BRACKET
                && tok.parent().map(|n| n.kind()) == Some(BLOCK);
            stack.push(Group {
                open: i,
                saw_newline: false,
                has_content: false,
                is_block,
            });
        } else if is_close(k) {
            if let Some(g) = stack.pop() {
                if g.saw_newline || (g.is_block && g.has_content) {
                    multiline[g.open] = true;
                    multiline[i] = true;
                    if let Some(parent) = stack.last_mut() {
                        parent.saw_newline = true; // a multi-line child spans the parent
                    }
                }
            }
        }
    }
    multiline
}

fn push_newline(out: &mut String, indent: usize, indent_width: usize, blank: bool) {
    // Trim any trailing spaces we just wrote (e.g. after `{ ` on an empty line).
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('\n');
    if blank {
        out.push('\n');
    }
    for _ in 0..indent * indent_width {
        out.push(' ');
    }
}

fn push_token(out: &mut String, tok: &SyntaxToken) {
    // Line comments may carry trailing whitespace before the newline; trim it.
    if tok.kind() == LINE_COMMENT {
        out.push_str(tok.text().trim_end());
    } else {
        out.push_str(tok.text());
    }
}

fn is_open(k: SyntaxKind) -> bool {
    matches!(k, L_BRACE | L_BRACKET | L_PAREN)
}

fn is_close(k: SyntaxKind) -> bool {
    matches!(k, R_BRACE | R_BRACKET | R_PAREN)
}

fn is_comment(k: SyntaxKind) -> bool {
    matches!(k, LINE_COMMENT | BLOCK_COMMENT)
}

fn is_op(k: SyntaxKind) -> bool {
    matches!(
        k,
        EQ | NOT_EQ
            | LT
            | GT
            | LT_EQ
            | GT_EQ
            | PLUS
            | MINUS
            | STAR
            | SLASH
            | PIPE_PIPE
            | ASSIGN
            | ARROW
    )
}
