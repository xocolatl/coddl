#!/usr/bin/env sh
# tools/check-grammar.sh
#
# Verify the docs/ tree is in lockstep with the syntax, types, and
# procir crates.
#
# Three invariants:
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
#   3. The keyword inventory syncs BIDIRECTIONALLY: every keyword or
#      glyph in crates/coddl-syntax/src/keywords.rs (outside tests)
#      appears backticked in grammar.md's "Reserved words" section, and
#      every backticked word/glyph on that section's inventory lines
#      (table rows and the bold Tier-3 list lines) exists in
#      keywords.rs. Which tier/table a word belongs to remains
#      documentation discipline, like the per-file scoping of checks
#      1-2. The VSCode TextMate grammar is held to the same standard,
#      both directions: every word its keyword patterns highlight
#      exists in keywords.rs, and every .cd keyword (the dialect sets
#      excluded) is highlighted.
#
# Exits 0 if all invariants hold, 1 with a summary of what's missing
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
# PB (.cddb parser), PM (.cdmap parser), PS (.cdstore parser),
# PL (.cd project plan, coddl-plan crate).
code_sources=$(
    find "$ROOT/crates/coddl-syntax/src" \
         "$ROOT/crates/coddl-types/src" \
         "$ROOT/crates/coddl-procir/src" \
         "$ROOT/crates/coddl-plan/src" \
        -name '*.rs' -type f 2>/dev/null
)
codes=$(
    grep -hoE '"(P[BMSL]|[EPTL])[0-9]{4}"' $code_sources 2>/dev/null \
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

# 3. Keyword inventory.
#
# keywords.rs is the single source of truth for every contextually
# recognized identifier; grammar.md's "Reserved words" section publishes
# it as the keyword taxonomy. Diff the two bidirectionally. The source
# harvest relies on the keywords.rs invariant that every string literal
# outside the test module is a keyword or glyph (stated in that file's
# module docs).

keywords_rs="$ROOT/crates/coddl-syntax/src/keywords.rs"
grammar_main="$ROOT/docs/grammar.md"

src_words=$(
    sed '/^#\[cfg(test)\]/,$d' "$keywords_rs" \
        | grep -v '^[[:space:]]*//' \
        | grep -oE '"[^"]+"' \
        | tr -d '"' \
        | sort -u
)
if [ -z "$src_words" ]; then
    echo "check-grammar: no keywords harvested from keywords.rs" >&2
    exit 1
fi

# The "Reserved words" section body (heading and next ## heading dropped).
section=$(
    sed -n '/^## Reserved words$/,/^## /p' "$grammar_main" | sed '1d;$d'
)
if [ -z "$section" ]; then
    echo "check-grammar: no \"Reserved words\" section found in grammar.md" >&2
    exit 1
fi

# 3a. Source -> doc: every keyword/glyph appears backticked somewhere in
# the section (multi-word entries like "not matching" included).
old_ifs=$IFS
IFS='
'
for w in $src_words; do
    if ! printf '%s\n' "$section" | grep -qF "\`$w\`"; then
        echo "check-grammar: keyword \`$w\` (keywords.rs) is missing from grammar.md \"Reserved words\"" >&2
        failed=1
    fi
done

# 3b. Doc -> source: every backticked word/glyph on the section's
# inventory lines (table rows `|...` and the bold `**...` list lines)
# exists in keywords.rs. Compound examples (tokens with spaces),
# punctuation-bearing code fragments (`:=`, `<assign-stmt>`, `[asc]`),
# parser fn names, and diagnostic codes are prose, not inventory.
doc_words=$(
    printf '%s\n' "$section" \
        | grep -E '^(\||\*\*)' \
        | grep -oE '`[^`]+`' \
        | tr -d '`' \
        | sort -u
)
for w in $doc_words; do
    case $w in
        *' '*) continue ;;
        parse_*) continue ;;
        [EPTL][0-9][0-9][0-9][0-9] | P[BMSL][0-9][0-9][0-9][0-9]) continue ;;
        *[\<\>\[\]\{\}\(\)\;\:\=\.\,\*\/\+\|\"\'\?\!]*) continue ;;
    esac
    if ! printf '%s\n' "$src_words" | grep -qxF "$w"; then
        echo "check-grammar: \`$w\` in a grammar.md \"Reserved words\" table is not in keywords.rs" >&2
        failed=1
    fi
done

# 3c/3d. TextMate grammar cross-check — the third keyword copy. Harvest
# every alternation from a "match" that directly follows a keyword.* /
# constant.language.boolean / support.type.generator scope name
# (stripping `\b` anchors, parens, and the `\s+` of the `all but` pair);
# symbol patterns from the operator scopes fall out via the punctuation
# filter. reltrue/relfalse (library constants) and the builtin scalar
# type names are deliberately outside the harvest — they are not
# keywords and are highlighted as vocabulary.
tm_json="$ROOT/editors/vscode/syntaxes/coddl.tmLanguage.json"
if [ ! -f "$tm_json" ]; then
    echo "check-grammar: editors/vscode/syntaxes/coddl.tmLanguage.json not found" >&2
    exit 1
fi
tm_words=$(
    grep -A1 -E '"name": "(keyword\.|storage\.|constant\.language\.boolean|support\.type\.generator)' "$tm_json" \
        | grep '"match"' \
        | sed 's/.*"match": "//; s/".*//' \
        | sed 's/\\\\b//g; s/[()]//g; s/\\\\s+/|/g' \
        | tr '|' '\n' \
        | sort -u
)

# The .cd keyword set: everything in keywords.rs above the dialect
# groups (CDDB/CDSTORE/CDMAP stay last in that file — a stated layout
# invariant there). The grammar's fileTypes is .cd only, so
# dialect-only words are not expected to be highlighted.
cd_words=$(
    sed '/^pub const CDDB_WORDS/,$d' "$keywords_rs" \
        | grep -v '^[[:space:]]*//' \
        | grep -oE '"[^"]+"' \
        | tr -d '"' \
        | sort -u
)

# tm -> source: no fictional keywords highlighted.
for w in $tm_words; do
    case $w in
        *' '*) continue ;;
        *[-\\\<\>\[\]\{\}\(\)\;\:\=\.\,\*\/\+\|\"\'\?\!]*) continue ;;
    esac
    if ! printf '%s\n' "$src_words" | grep -qxF "$w"; then
        echo "check-grammar: \`$w\` is highlighted by coddl.tmLanguage.json but is not in keywords.rs" >&2
        failed=1
    fi
done

# source -> tm: every .cd keyword is highlighted. Multi-word entries
# ("not matching") are covered by their component words.
for w in $cd_words; do
    case $w in
        *' '*) continue ;;
    esac
    if ! printf '%s\n' "$tm_words" | grep -qxF "$w"; then
        echo "check-grammar: .cd keyword \`$w\` (keywords.rs) is not highlighted by coddl.tmLanguage.json" >&2
        failed=1
    fi
done
IFS=$old_ifs

if [ "$failed" -eq 0 ]; then
    echo "check-grammar: docs/ is in sync with crates/coddl-syntax + crates/coddl-types + crates/coddl-procir + crates/coddl-plan"
fi

exit "$failed"
