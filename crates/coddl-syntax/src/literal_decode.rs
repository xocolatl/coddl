//! Canonical scalar-literal decoders.
//!
//! One home for turning a validated literal *lexeme* (the text of an
//! `INTEGER_LIT` / `STRING_LIT` / `CHAR_LIT` / `RATIONAL_LIT` / `APPROXIMATE_LIT`
//! token) into its Coddl scalar *value*. Every one of these assumes the lexer
//! has already validated the literal's form — they decode, they don't re-check —
//! so a malformed lexeme is a lexer bug, surfaced here as a panic, never a
//! user-visible error.
//!
//! These live in `coddl-syntax` (not the lowerer) because more than one consumer
//! needs them: ProcIR lowering (`coddl-procir`) folds them into `Const`/`RelLiteral`
//! payloads, and catalog INIT evaluation (`coddl provision`) decodes the same
//! ground literals from a `.cddb`. Keeping a single decoder means the two paths
//! can never disagree on what `3.4` or `0x2a` or `'\u{1F600}'` means.

/// Decode the body of a `STRING_LIT` token (with surrounding `"`s) to its raw
/// UTF-8 bytes, expanding the escape set the lexer accepts (`\n`, `\r`, `\t`,
/// `\"`, `\\`, `\u{...}`).
pub fn decode_string_literal(text: &str) -> Vec<u8> {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    let mut out = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(esc) = chars.next() else { break };
        match esc {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '"' => out.push(b'"'),
            '\\' => out.push(b'\\'),
            'u' => {
                // `\u{XXXX}` — the lexer already validated the form.
                if chars.next() != Some('{') {
                    break;
                }
                let mut hex = String::new();
                for h in chars.by_ref() {
                    if h == '}' {
                        break;
                    }
                    hex.push(h);
                }
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            _ => unreachable!("unknown escape `\\{esc}` survived lexing"),
        }
    }
    out
}

/// Decode the body of a `CHAR_LIT` token (with surrounding `'`s) to its
/// Unicode scalar value. The lexer guarantees exactly one codepoint and the
/// same escape set as `STRING_LIT` (`\n`, `\r`, `\t`, `\"`, `\\`, `\u{...}`).
pub fn decode_char_literal(text: &str) -> u32 {
    let inner = text
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(text);
    let mut chars = inner.chars();
    let c = chars.next().expect("lexer rejects empty char literal");
    if c != '\\' {
        return c as u32;
    }
    match chars.next().expect("lexer rejects a lone backslash") {
        'n' => '\n' as u32,
        'r' => '\r' as u32,
        't' => '\t' as u32,
        '"' => '"' as u32,
        '\'' => '\'' as u32,
        '\\' => '\\' as u32,
        'u' => {
            // `\u{XXXX}` — the lexer already validated the form.
            debug_assert_eq!(chars.next(), Some('{'));
            let hex: String = chars.by_ref().take_while(|h| *h != '}').collect();
            u32::from_str_radix(&hex, 16).expect("lexer validated the codepoint")
        }
        esc => unreachable!("unknown escape `\\{esc}` survived lexing"),
    }
}

/// Parse an `INTEGER_LIT` lexeme into its `i64` value. Handles the
/// four bases the lexer recognizes (`0x`, `0b`, `0o`, `0d`) plus the
/// default decimal form. Underscores between digits are stripped.
pub fn parse_integer_literal(text: &str) -> i64 {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        (2, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        (8, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0d")
        .or_else(|| cleaned.strip_prefix("0D"))
    {
        (10, rest)
    } else {
        (10, cleaned.as_str())
    };
    i64::from_str_radix(digits, radix).expect("lexer validated the digits")
}

/// Collapse an `f64` to the canonical IEEE-754 bit pattern for its Coddl
/// `Approximate` *value*: all NaNs → one quiet-NaN pattern, `−0.0` → `+0.0`,
/// everything else its own bits. This is what makes bitwise equality a proper
/// (reflexive) equality — the same rule the runtime applies on SQL read-back.
/// Mirror this rule anywhere else an `Approximate` enters the system.
pub fn canonical_approx_bits(x: f64) -> u64 {
    if x.is_nan() {
        f64::NAN.to_bits()
    } else if x == 0.0 {
        // `x == 0.0` is true for both `+0.0` and `−0.0`; collapse to `+0.0`.
        0
    } else {
        x.to_bits()
    }
}

/// Decode an `Approximate` literal (`42e0`, `4.2e1`, `1e-9`) to its canonical
/// bit pattern. Underscores are decoration (stripped like `parse_integer_literal`);
/// the lexer already validated the mantissa/exponent shape.
pub fn decode_approximate_literal(text: &str) -> u64 {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let value: f64 = cleaned
        .parse()
        .expect("lexer validated the approximate literal");
    canonical_approx_bits(value)
}

/// Greatest common divisor of two `i128` magnitudes (Euclid). `gcd(0, x) = |x|`.
pub fn gcd_i128(mut a: i128, mut b: i128) -> i128 {
    a = a.abs();
    b = b.abs();
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Reduce `(n, d)` to the canonical `i64` `Rational`: `gcd(|n|, d) = 1`,
/// `d > 0`, `0 → (0, 1)`. Reduces in `i128` (so decode's `10^k` intermediate
/// can't wrap) then narrows to `i64`. Panics on `d == 0` (division by zero is
/// not a rational) and on a reduced component that exceeds `i64` (a literal past
/// the bounded type's range). Every compile-time `Rational` funnels through
/// this; the runtime mirror (`reduce_to_i64`) handles the same narrowing.
pub fn reduce_rational(n: i128, d: i128) -> (i64, i64) {
    assert!(d != 0, "rational with zero denominator");
    if n == 0 {
        return (0, 1);
    }
    let g = gcd_i128(n, d);
    let (mut n, mut d) = (n / g, d / g);
    if d < 0 {
        n = -n;
        d = -d;
    }
    let narrow = |v: i128| i64::try_from(v).expect("rational component exceeds i64");
    (narrow(n), narrow(d))
}

/// Decode a `Rational` literal (`3.4`, `42.0`, `3.1415926`) — the lexer's
/// `digits . digits` form — to its reduced `(numer, denom)` pair. `d.ffff` with
/// `k` fractional digits → `(all_digits, 10^k)`, reduced. Underscores are
/// decoration. A literal whose reduced form exceeds `i64` (≳19 digits) traps.
pub fn decode_rational_literal(text: &str) -> (i64, i64) {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let (int_part, frac_part) = match cleaned.split_once('.') {
        Some((i, f)) => (i, f),
        None => (cleaned.as_str(), ""),
    };
    let digits: String = format!("{int_part}{frac_part}");
    let numer: i128 = digits
        .parse()
        .expect("rational literal numerator exceeds i128");
    let denom: i128 = 10i128
        .checked_pow(frac_part.len() as u32)
        .expect("rational literal denominator exceeds i128");
    reduce_rational(numer, denom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_literal_decodes_decimal_and_hex() {
        assert_eq!(parse_integer_literal("42"), 42);
        assert_eq!(parse_integer_literal("0x2a"), 42);
        assert_eq!(parse_integer_literal("0b101010"), 42);
        assert_eq!(parse_integer_literal("0o52"), 42);
        assert_eq!(parse_integer_literal("1_000"), 1000);
    }

    #[test]
    fn rational_literal_decodes_to_reduced_pair() {
        assert_eq!(decode_rational_literal("3.4"), (17, 5));
        assert_eq!(decode_rational_literal("42.0"), (42, 1));
        assert_eq!(decode_rational_literal("0.5"), (1, 2));
        assert_eq!(decode_rational_literal("3.1415926"), (15707963, 5000000));
        assert_eq!(decode_rational_literal("1_000.0"), (1000, 1));
        // gcd/normalize edge: already-canonical stays; zero is (0,1).
        assert_eq!(decode_rational_literal("0.0"), (0, 1));
    }

    #[test]
    fn string_literal_expands_escapes() {
        assert_eq!(decode_string_literal("\"hi\""), b"hi");
        assert_eq!(decode_string_literal("\"a\\nb\""), b"a\nb");
        assert_eq!(decode_string_literal("\"q\\\"q\""), b"q\"q");
        // `\u{...}` expands to its UTF-8 bytes.
        assert_eq!(decode_string_literal("\"\\u{1F600}\""), "😀".as_bytes());
    }

    #[test]
    fn char_literal_decodes_codepoint_and_escapes() {
        assert_eq!(decode_char_literal("'a'"), 'a' as u32);
        assert_eq!(decode_char_literal("'\\n'"), '\n' as u32);
        assert_eq!(decode_char_literal("'\\u{1F600}'"), 0x1F600);
    }

    #[test]
    fn approximate_literal_canonicalizes() {
        assert_eq!(decode_approximate_literal("42e0"), 42.0f64.to_bits());
        assert_eq!(decode_approximate_literal("4.2e1"), 42.0f64.to_bits());
        // `−0.0` collapses to `+0.0` so bitwise equality is reflexive.
        assert_eq!(canonical_approx_bits(-0.0), 0);
        assert_eq!(canonical_approx_bits(f64::NAN), f64::NAN.to_bits());
    }
}
