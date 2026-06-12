#!/usr/bin/env sh
# tools/check-grammar.sh
#
# Verify the docs/ tree is in lockstep with the syntax, types, and
# procir crates.
#
# Two invariants:
#   1. Every `fn parse_<name>` in any parser*.rs (outside #[cfg(test)]
#      code) has a `<name>` rule (kebab-case) defined in some
#      docs/*-grammar.md or docs/grammar.md file. Which doc file owns
#      which rule is documentation discipline (the .cd parser writes to
#      grammar.md; dialect parsers write to their per-dialect docs);
#      the script only enforces that every parser function has a rule
#      *somewhere* under docs/.
#   2. Every `"E####"` / `"P####"` / `"T####"` / `"L####"` / `"PB####"`
#      / `"PM####"` / `"PS####"` diagnostic code emitted anywhere in
#      crates/coddl-syntax/src/, crates/coddl-types/src/, or
#      crates/coddl-procir/src/ appears in some docs/*.md file. Which
#      file owns which code prefix is documentation discipline; the
#      script only enforces that every emitted code is documented
#      somewhere.
#
# Exits 0 if both invariants hold, 1 with a summary of what's missing
# otherwise. POSIX shell only — no Python, no Rust.

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)

# All parser source files (one per dialect, plus the .cd parser).
parser_files=$(
    find "$ROOT/crates/coddl-syntax/src" \
        -maxdepth 1 -name 'parser*.rs' -type f 2>/dev/null
)
if [ -z "$parser_files" ]; then
    echo "check-grammar: no parser source found under crates/coddl-syntax/src" >&2
    exit 1
fi

# All grammar docs (the main .cd grammar plus per-dialect grammars).
grammar_docs=$(
    find "$ROOT/docs" -maxdepth 1 -name '*grammar.md' -type f 2>/dev/null
)
if [ -z "$grammar_docs" ]; then
    echo "check-grammar: no grammar docs found under docs/" >&2
    exit 1
fi

failed=0

# 1. parse_<name> functions outside the test module across every parser
# source file. Each must have a corresponding kebab-case <name> rule in
# at least one of the grammar docs.
parser_fns=$(
    for p in $parser_files; do
        sed '/^#\[cfg(test)\]/,$d' "$p" \
            | grep -oE 'fn parse_[a-z_]+' \
            | sed 's/^fn parse_//'
    done | sort -u
)

for fn in $parser_fns; do
    rule=$(printf '%s' "$fn" | tr '_' '-')
    if ! grep -lqE "<$rule>" $grammar_docs 2>/dev/null; then
        echo "check-grammar: missing grammar rule <$rule>  (parser fn: parse_$fn)" >&2
        failed=1
    fi
done

# 2. Diagnostic codes.
#
# Harvest every `"X####"` literal across the syntax, types, and procir
# crates; verify each appears in some docs/*.md. Recognized prefixes:
# E (lexer), P (.cd parser), T (typechecker), L (lower),
# PB (.cddb parser), PM (.cdmap parser), PS (.cdstore parser).
code_sources=$(
    find "$ROOT/crates/coddl-syntax/src" \
         "$ROOT/crates/coddl-types/src" \
         "$ROOT/crates/coddl-procir/src" \
        -name '*.rs' -type f 2>/dev/null
)
codes=$(
    grep -hoE '"(P[BMS]|[EPTL])[0-9]{4}"' $code_sources 2>/dev/null \
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
    echo "check-grammar: docs/ is in sync with crates/coddl-syntax + crates/coddl-types + crates/coddl-procir"
fi

exit "$failed"
