#!/usr/bin/env sh
# tools/check-grammar.sh
#
# Verify the docs/ tree is in lockstep with the syntax and types crates.
#
# Two invariants:
#   1. Every `fn parse_<name>` in parser.rs (outside #[cfg(test)] code)
#      has a `<name>` rule (kebab-case) defined in docs/grammar.md.
#   2. Every `"E####"` / `"P####"` / `"T####"` diagnostic code emitted
#      anywhere in crates/coddl-syntax/src/ or crates/coddl-types/src/
#      appears in some docs/*.md file. Which file owns which code prefix
#      is documentation discipline; the script only enforces that
#      every emitted code is documented somewhere.
#
# Exits 0 if both invariants hold, 1 with a summary of what's missing
# otherwise. POSIX shell only — no Python, no Rust.

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
GRAMMAR="$ROOT/docs/grammar.md"
PARSER="$ROOT/crates/coddl-syntax/src/parser.rs"

if [ ! -f "$GRAMMAR" ]; then
    echo "check-grammar: docs/grammar.md not found at $GRAMMAR" >&2
    exit 1
fi
if [ ! -f "$PARSER" ]; then
    echo "check-grammar: parser source not found at $PARSER" >&2
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
# Harvest every `"X####"` literal across the syntax and types crates;
# verify each appears in some docs/*.md.
code_sources=$(
    find "$ROOT/crates/coddl-syntax/src" "$ROOT/crates/coddl-types/src" \
        -name '*.rs' -type f 2>/dev/null
)
codes=$(
    grep -hoE '"[EPT][0-9]{4}"' $code_sources 2>/dev/null \
        | tr -d '"' \
        | sort -u
)

# All docs/*.md files. Joined as space-separated for the grep below.
doc_files=$(find "$ROOT/docs" -maxdepth 1 -name '*.md' -type f 2>/dev/null)

for code in $codes; do
    if ! grep -lqE "(^| )$code( |\$|\|)" $doc_files 2>/dev/null; then
        echo "check-grammar: diagnostic $code is emitted but not documented" >&2
        failed=1
    fi
done

if [ "$failed" -eq 0 ]; then
    echo "check-grammar: docs/ is in sync with crates/coddl-syntax + crates/coddl-types"
fi

exit "$failed"
