//! Compile-time constant folding of scalar expressions: `Expr → Literal`.
//!
//! Evaluating a constant scalar expression (`2 * 3 + 1`, `-12.0`, `"a" || "b"`)
//! is a frontend semantic primitive, upstream of both procedural-IR lowering
//! and catalog provisioning. It lives here — above `coddl-syntax` (the AST it
//! folds) and `coddl-relir` (the [`Literal`] it produces) and below every crate
//! that needs it — so those consumers depend *downward* on one shared folder
//! rather than reaching sideways into each other. `coddl-procir` uses it for
//! module-`let` constants and pushdown-predicate values; `coddl-provision` uses
//! it to evaluate `.cddb` INIT cells into seed rows.
//!
//! The arithmetic is the compile-time mirror of the runtime `ScalarOp`
//! semantics: checked Integer arithmetic, i128-intermediate Rational arithmetic
//! with the runtime's reduce/narrow rules, canonical-bit Approximate equality,
//! content Text comparison, `||` over Text/Character. A value that doesn't exist
//! (overflow, division by zero) is an `Err`, never a silent wrap.

use coddl_relir::Literal;
use coddl_syntax::ast::{BinaryOp, Expr, UnaryOp};
use coddl_syntax::literal_decode::{
    decode_approximate_literal, decode_char_literal, decode_rational_literal,
    decode_string_literal, gcd_i128, parse_integer_literal,
};
use coddl_syntax::SyntaxKind;

/// Fold a scalar expression to its constant [`Literal`], or `Ok(None)` when
/// `expr` isn't a scalar constant (a relation literal, a relational operator, a
/// name that doesn't resolve to a constant). `Err` when a scalar constant's
/// value doesn't exist (overflow, division by zero) — a hard error, never a
/// silent wrap.
///
/// `resolve` maps a bare name to its folded constant (e.g. a module-level `let`)
/// or `None` when the name isn't a compile-time constant in scope — a caller's
/// hook for its own name environment and shadowing rules. A caller with no
/// constant-name environment (a `.cddb` INIT cell) passes `|_| None`.
pub fn fold_const_scalar(
    expr: &Expr,
    resolve: &dyn Fn(&str) -> Option<Literal>,
) -> Result<Option<Literal>, String> {
    if let Some(lit) = literal_value(expr, resolve) {
        return Ok(Some(lit));
    }
    match expr {
        Expr::NameRef(n) => Ok(n.ident().and_then(|t| resolve(t.text()))),
        Expr::Unary(u) if u.op_kind() == Some(UnaryOp::Not) => {
            let Some(operand) = u.operand() else {
                return Ok(None);
            };
            match fold_const_scalar(&operand, resolve)? {
                Some(Literal::Boolean(b)) => Ok(Some(Literal::Boolean(!b))),
                _ => Ok(None),
            }
        }
        Expr::Unary(u) if matches!(u.op_kind(), Some(UnaryOp::Pos | UnaryOp::Neg)) => {
            let Some(operand) = u.operand() else {
                return Ok(None);
            };
            let Some(lit) = fold_const_scalar(&operand, resolve)? else {
                return Ok(None);
            };
            // `+x` is the identity; `-x` folds as `0 - x` — the same
            // overflow/reduce semantics as the runtime and the lowering desugar
            // (Rational stays canonical: den > 0, sign on the numerator).
            match (u.op_kind(), lit) {
                (Some(UnaryOp::Pos), l @ (Literal::Integer(_) | Literal::Rational(..))) => {
                    Ok(Some(l))
                }
                (Some(UnaryOp::Neg), l @ Literal::Integer(_)) => {
                    fold_binary_const(Some(BinaryOp::Sub), Literal::Integer(0), l)
                }
                (Some(UnaryOp::Neg), l @ Literal::Rational(..)) => {
                    fold_binary_const(Some(BinaryOp::Sub), Literal::Rational(0, 1), l)
                }
                _ => Ok(None),
            }
        }
        Expr::Binary(b) => {
            let (Some(le), Some(re)) = (b.lhs(), b.rhs()) else {
                return Ok(None);
            };
            let (Some(l), Some(r)) = (
                fold_const_scalar(&le, resolve)?,
                fold_const_scalar(&re, resolve)?,
            ) else {
                return Ok(None);
            };
            fold_binary_const(b.op_kind(), l, r)
        }
        _ => Ok(None),
    }
}

/// Convert a leaf literal — or a name resolving to a constant, or a
/// `<int> / <int>` rational constant — to a [`Literal`], or `None` for forms
/// this decoder doesn't bind. `resolve` supplies constant values for names (see
/// [`fold_const_scalar`]).
pub fn literal_value(expr: &Expr, resolve: &dyn Fn(&str) -> Option<Literal>) -> Option<Literal> {
    match expr {
        Expr::Literal(lit) => {
            let token = lit.token()?;
            match token.kind() {
                SyntaxKind::INTEGER_LIT => {
                    Some(Literal::Integer(parse_integer_literal(token.text())))
                }
                SyntaxKind::STRING_LIT => {
                    let bytes = decode_string_literal(token.text());
                    String::from_utf8(bytes).ok().map(Literal::Text)
                }
                SyntaxKind::CHAR_LIT => Some(Literal::Character(decode_char_literal(token.text()))),
                SyntaxKind::APPROXIMATE_LIT => Some(Literal::Approximate(
                    decode_approximate_literal(token.text()),
                )),
                SyntaxKind::RATIONAL_LIT => {
                    let (n, d) = decode_rational_literal(token.text());
                    Some(Literal::Rational(n, d))
                }
                _ => None,
            }
        }
        Expr::BoolLit(b) => b.value().map(Literal::Boolean),
        // A name is a constant only if `resolve` says so (a module-level `let`,
        // not shadowed by a local — the caller's closure decides).
        Expr::NameRef(n) => resolve(n.ident()?.text()),
        // Exact `/` of two Integer literals is a compile-time Rational constant
        // (there is no `2/3` literal token). `try_reduce_rational` declines
        // `d == 0` and any i64-overflow — `.ok()` turns that into `None`, so the
        // fold is simply unavailable rather than a panic.
        Expr::Binary(b) if b.op_kind() == Some(BinaryOp::Div) => {
            let n = int_literal_i64(&b.lhs()?)?;
            let d = int_literal_i64(&b.rhs()?)?;
            try_reduce_rational(n as i128, d as i128)
                .ok()
                .map(|(n, d)| Literal::Rational(n, d))
        }
        _ => None,
    }
}

/// If `expr` is an integer literal, its `i64` value; else `None`.
fn int_literal_i64(expr: &Expr) -> Option<i64> {
    if let Expr::Literal(lit) = expr {
        let tok = lit.token()?;
        if tok.kind() == SyntaxKind::INTEGER_LIT {
            return Some(parse_integer_literal(tok.text()));
        }
    }
    None
}

/// `reduce_rational` with a graceful narrow: `Err` when the reduced component
/// exceeds `i64` — the compile-time mirror of the runtime's narrowing trap,
/// surfaced as an error instead of a panic.
pub fn try_reduce_rational(n: i128, d: i128) -> Result<(i64, i64), String> {
    if d == 0 {
        return Err("divides by zero".to_string());
    }
    if n == 0 {
        return Ok((0, 1));
    }
    let g = gcd_i128(n, d);
    let (mut n, mut d) = (n / g, d / g);
    if d < 0 {
        n = -n;
        d = -d;
    }
    let narrow =
        |v: i128| i64::try_from(v).map_err(|_| "overflows Rational (i64 component)".to_string());
    Ok((narrow(n)?, narrow(d)?))
}

/// Evaluate one built-in binary operator over two folded scalar constants — the
/// compile-time mirror of the runtime `ScalarOp` semantics (checked Integer
/// arithmetic; exact `/` to Rational; i128-intermediate Rational arithmetic with
/// the runtime's reduce/narrow rules; cross-multiply Rational ordering; content
/// Text equality; canonical-bit Approximate equality; `||` over Text/Character).
/// `Ok(None)` for a shape the folder doesn't cover (relational operators,
/// `where`, mixed kinds the checker would have rejected); `Err` when the value
/// doesn't exist (overflow, division by zero) — a hard error, never a silent
/// wrap.
pub fn fold_binary_const(
    op: Option<BinaryOp>,
    l: Literal,
    r: Literal,
) -> Result<Option<Literal>, String> {
    use Literal as L;
    let out = match (op, l, r) {
        (Some(BinaryOp::Add), L::Integer(a), L::Integer(b)) => {
            L::Integer(a.checked_add(b).ok_or("`+` overflows Integer")?)
        }
        (Some(BinaryOp::Sub), L::Integer(a), L::Integer(b)) => {
            L::Integer(a.checked_sub(b).ok_or("`-` overflows Integer")?)
        }
        (Some(BinaryOp::Mul), L::Integer(a), L::Integer(b)) => {
            L::Integer(a.checked_mul(b).ok_or("`*` overflows Integer")?)
        }
        (Some(BinaryOp::IntDiv), L::Integer(a), L::Integer(b)) => {
            if b == 0 {
                return Err("`div` divides by zero".to_string());
            }
            L::Integer(a.checked_div(b).ok_or("`div` overflows Integer")?)
        }
        // Exact `/`: Integer × Integer → Rational.
        (Some(BinaryOp::Div), L::Integer(a), L::Integer(b)) => {
            let (n, d) =
                try_reduce_rational(a as i128, b as i128).map_err(|e| format!("`/` {e}"))?;
            L::Rational(n, d)
        }
        // Rational arithmetic: i128 intermediates never wrap before the reduce;
        // the narrow mirrors the runtime trap.
        (Some(BinaryOp::Add), L::Rational(an, ad), L::Rational(bn, bd)) => {
            let n = an as i128 * bd as i128 + bn as i128 * ad as i128;
            let (n, d) =
                try_reduce_rational(n, ad as i128 * bd as i128).map_err(|e| format!("`+` {e}"))?;
            L::Rational(n, d)
        }
        (Some(BinaryOp::Sub), L::Rational(an, ad), L::Rational(bn, bd)) => {
            let n = an as i128 * bd as i128 - bn as i128 * ad as i128;
            let (n, d) =
                try_reduce_rational(n, ad as i128 * bd as i128).map_err(|e| format!("`-` {e}"))?;
            L::Rational(n, d)
        }
        (Some(BinaryOp::Mul), L::Rational(an, ad), L::Rational(bn, bd)) => {
            let (n, d) = try_reduce_rational(an as i128 * bn as i128, ad as i128 * bd as i128)
                .map_err(|e| format!("`*` {e}"))?;
            L::Rational(n, d)
        }
        (Some(BinaryOp::Div), L::Rational(an, ad), L::Rational(bn, bd)) => {
            if bn == 0 {
                return Err("`/` divides by zero".to_string());
            }
            let (n, d) = try_reduce_rational(an as i128 * bd as i128, ad as i128 * bn as i128)
                .map_err(|e| format!("`/` {e}"))?;
            L::Rational(n, d)
        }
        (Some(BinaryOp::And), L::Boolean(a), L::Boolean(b)) => L::Boolean(a && b),
        (Some(BinaryOp::Or), L::Boolean(a), L::Boolean(b)) => L::Boolean(a || b),
        (Some(op @ (BinaryOp::Eq | BinaryOp::NotEq)), l, r) => {
            // Same-kind operands (typechecked). Reduced Rational pairs and
            // canonical Approximate bits make structural equality
            // value-equality — the same rule the runtime cells follow.
            let eq = match (&l, &r) {
                (L::Integer(a), L::Integer(b)) => a == b,
                (L::Text(a), L::Text(b)) => a == b,
                (L::Character(a), L::Character(b)) => a == b,
                (L::Approximate(a), L::Approximate(b)) => a == b,
                (L::Rational(an, ad), L::Rational(bn, bd)) => an == bn && ad == bd,
                (L::Boolean(a), L::Boolean(b)) => a == b,
                _ => return Ok(None),
            };
            L::Boolean(if op == BinaryOp::Eq { eq } else { !eq })
        }
        (Some(op @ (BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq)), l, r) => {
            // Ordering is Integer/Rational only (typechecked); Rational compares
            // by cross-multiplication (denominators positive).
            let ord = match (&l, &r) {
                (L::Integer(a), L::Integer(b)) => a.cmp(b),
                (L::Rational(an, ad), L::Rational(bn, bd)) => {
                    (*an as i128 * *bd as i128).cmp(&(*bn as i128 * *ad as i128))
                }
                _ => return Ok(None),
            };
            L::Boolean(match op {
                BinaryOp::Lt => ord.is_lt(),
                BinaryOp::Gt => ord.is_gt(),
                BinaryOp::LtEq => ord.is_le(),
                _ => ord.is_ge(),
            })
        }
        (Some(BinaryOp::Concat), l, r) => {
            let text_of = |v: &L| -> Option<String> {
                match v {
                    L::Text(s) => Some(s.clone()),
                    L::Character(cp) => char::from_u32(*cp).map(String::from),
                    _ => None,
                }
            };
            match (text_of(&l), text_of(&r)) {
                (Some(a), Some(b)) => L::Text(a + &b),
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    // The `Literal`-level arithmetic core (`fold_binary_const` /
    // `try_reduce_rational`) is tested directly here — no parser needed. The
    // `Expr`-level entry points (`fold_const_scalar` / `literal_value`) get
    // end-to-end coverage from `coddl-procir` (module-`let` folding) and
    // `coddl-provision` (INIT-cell evaluation), which parse real source.
    use super::*;

    #[test]
    fn integer_arithmetic_is_checked() {
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::Add),
                Literal::Integer(2),
                Literal::Integer(3)
            ),
            Ok(Some(Literal::Integer(5)))
        );
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::Mul),
                Literal::Integer(i64::MAX),
                Literal::Integer(2)
            ),
            Err("`*` overflows Integer".to_string())
        );
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::IntDiv),
                Literal::Integer(1),
                Literal::Integer(0)
            ),
            Err("`div` divides by zero".to_string())
        );
    }

    #[test]
    fn exact_division_yields_reduced_rational() {
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::Div),
                Literal::Integer(6),
                Literal::Integer(4)
            ),
            Ok(Some(Literal::Rational(3, 2)))
        );
        // Sign lands on the numerator, denominator stays positive.
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::Div),
                Literal::Integer(1),
                Literal::Integer(-2)
            ),
            Ok(Some(Literal::Rational(-1, 2)))
        );
    }

    #[test]
    fn rational_arithmetic_reduces_via_i128_intermediates() {
        // 1/2 + 1/3 = 5/6.
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::Add),
                Literal::Rational(1, 2),
                Literal::Rational(1, 3),
            ),
            Ok(Some(Literal::Rational(5, 6)))
        );
    }

    #[test]
    fn try_reduce_rational_declines_zero_denominator_and_overflow() {
        assert!(try_reduce_rational(1, 0).is_err());
        assert_eq!(try_reduce_rational(0, 5), Ok((0, 1)));
        // A denominator that can't narrow to i64 is an error, not a panic.
        assert!(try_reduce_rational(1, i128::from(i64::MAX) + 1).is_err());
    }

    #[test]
    fn concat_joins_text_and_characters() {
        assert_eq!(
            fold_binary_const(
                Some(BinaryOp::Concat),
                Literal::Text("ab".to_string()),
                Literal::Character('c' as u32),
            ),
            Ok(Some(Literal::Text("abc".to_string())))
        );
    }
}
