#!/usr/bin/env sh
# tools/check-grammar.sh
#
# Verify docs/grammar.md is in lockstep with crates/coddl-syntax.
#
# Two invariants:
#   1. Every `fn parse_<name>` in parser.rs (outside #[cfg(test)] code)
#      has a `<name>` rule (kebab-case) defined in docs/grammar.md.
#   2. Every `"E####"` / `"P####"` diagnostic code emitted in the
#      syntax crate appears in docs/grammar.md.
#
# Exits 0 if both invariants hold, 1 with a summary of what's missing
# otherwise. POSIX shell only — no Python, no Rust.

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
GRAMMAR="$ROOT/docs/grammar.md"
PARSER="$ROOT/crates/coddl-syntax/src/parser.rs"
LEXER="$ROOT/crates/coddl-syntax/src/lexer.rs"

if [ ! -f "$GRAMMAR" ]; then
    echo "check-grammar: docs/grammar.md not found at $GRAMMAR" >&2
    exit 1
fi
if [ ! -f "$PARSER" ]; then
    echo "check-grammar: parser source not found at $PARSER" >&2
    exit 1
fi
if [ ! -f "$LEXER" ]; then
    echo "check-grammar: lexer source not found at $LEXER" >&2
    exit 1
fi

failed=0

# 1. parse_<name> functions outside the test module.
#
# Strip everything from the first `#[cfg(test)]` onward; what remains
# is parser source proper. Then collect `fn parse_<name>` symbols and
# verify each has a corresponding kebab-case `<name>` rule defined in
# the grammar doc.
parser_fns=$(
    sed '/^#\[cfg(test)\]/,$d' "$PARSER" \
        | grep -oE 'fn parse_[a-z_]+' \
        | sed 's/^fn parse_//' \
        | sort -u
)

for fn in $parser_fns; do
    rule=$(printf '%s' "$fn" | tr '_' '-')
    if ! grep -qE "<$rule>" "$GRAMMAR"; then
        echo "check-grammar: missing grammar rule <$rule>  (parser fn: parse_$fn)" >&2
        failed=1
    fi
done

# 2. Diagnostic codes.
#
# Every `"E####"` / `"P####"` literal in the syntax crate sources must
# show up somewhere in the grammar doc. We harvest test assertions too
# (`d.code == "P0017"`), since they pin down which codes are exercised
# and act as a cross-check.
codes=$(
    grep -hoE '"[EP][0-9]{4}"' "$PARSER" "$LEXER" \
        | tr -d '"' \
        | sort -u
)

for code in $codes; do
    if ! grep -qE "(^| )$code( |\$|\|)" "$GRAMMAR"; then
        echo "check-grammar: diagnostic $code is emitted but not documented" >&2
        failed=1
    fi
done

if [ "$failed" -eq 0 ]; then
    echo "check-grammar: docs/grammar.md is in sync with crates/coddl-syntax"
fi

exit "$failed"
