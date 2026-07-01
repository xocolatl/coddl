# Code formatter (`coddl fmt`)

A canonical formatter for Coddl source, exposed two ways from one library: `coddl fmt` (driver subcommand, à la `cargo fmt` — see [driver.md](driver.md)) and `textDocument/formatting` in `coddl-lsp` (format-on-save — see [lsp.md](lsp.md)). Both paths call into `coddl-fmt`; there is no second implementation.

## CST over AST-with-trivia

This is the load-bearing decision. The compiler's typechecker doesn't care about whitespace or comments — they're noise for analysis. The formatter cares about every byte. Three options were considered:

1. **AST + side-channel trivia.** Parser emits the AST and a parallel list of (byte-range, trivia) entries. Formatter walks the AST and consults the list. Cheap up-front; every formatter pass re-decides "where does this comment attach?", and edge cases proliferate.
2. **AST with attached trivia.** Each AST node holds leading/trailing trivia. Bloats the AST for every consumer; non-formatter passes pay the memory cost.
3. **Concrete syntax tree (CST) + AST view.** Parser produces a lossless tree — every token, every trivia, every byte. The AST is a typed view derived from the CST; the typechecker walks the AST, the formatter walks the CST, both share the same backing storage. This is `rust-analyzer`'s approach via `rowan`.

**Coddl picks option 3.** This is one of the long-term-planning bills paid up front (see [principles.md](principles.md)): the formatter, the LSP semantic-tokens path, and incremental re-analysis under `salsa` (see [lsp.md](lsp.md)) all want a lossless tree. Retrofitting one is a parser rewrite — exactly the kind of corner-painting this project explicitly avoids. `coddl-syntax` produces a CST from day one; `coddl-types`, `coddl-relir`, and friends consume an AST view derived from it.

`coddl-diagnostics::Span` carries through unchanged — it's still `(file_id, byte_range)`, which the CST can produce for any node trivially.

## Formatting rules (v1)

The formatter is opinionated and has few knobs. A `fmt` whose output drifts between versions or wobbles with bikeshedding is worse than a stricter one. Initial rules:

- **Indent**: 4 spaces (`indent_width` config; revisit if real demand surfaces).
- **Line width**: 100 columns soft; hard if a single token can't be split.
- **Braces**: `{` on the same line as the keyword/operator that opens them; `}` on its own line aligned with the opener — except trivial single-line bodies (`OP{ x: 1, y: 2 }`) which stay inline up to the line-width limit.
- **Name-attached bracket lists glue to their name** — no space before the opener — for the three forms where the brackets *belong to* the preceding name: a call's argument list (`f{ … }`, including the dot-method form `x.m{ … }`), an `oper` declaration's parameter heading (`oper name{ … }`), and a sequence index (`s[0]`). This is a structural rule keyed on the CST node that owns the bracket, not on token adjacency: a `{ … }` tuple/relation literal value, a `Sequence [ … ]` literal, and a relvar/`Tuple`/`Relation` heading (the shared `HEADING` node in a non-`oper` position) are *not* name-attached and keep their leading space. Empty argument braces stay tight (`m{}`).
- **Named arguments inside braces**: one space after the colon (`name: value`), one space after the inter-arg comma. One per line if any single arg makes the whole call exceed the line width; otherwise stay on the line. No alignment of names or colons across lines (it churns under add/remove).
- **Operator spacing**: one space around `=`, `<`, `>`, `+`, `-`, `*`, `/`, `,`; no space around `.`.
- **Trailing commas**: required in multi-line bracketed lists, forbidden in single-line ones (so adding then removing a wrap is idempotent).
- **Blank lines**: preserve user blank lines between top-level items, collapsed to at most one consecutive blank.
- **Comments**: preserved as-is, attached to the following node by default. Block-leading `//` comments stay on their own line; trailing `//` comments stay trailing. `/* */` block comments (including nested ones — see [grammar.md](grammar.md)) keep their existing line breaks; the formatter doesn't reflow content inside them.

Idempotency is a unit-test invariant: `fmt(fmt(x)) == fmt(x)` for every input in `examples/` and `tests/`.

## Edition versioning

Formatter output is versioned, à la `rustfmt`'s editions. A project's `coddl.toml` carries `format.edition = "2026"` (or whichever); default = newest edition the compiler knows. Edition bumps are explicit opt-in; old projects keep their formatting until they update. This buys the freedom to evolve the rules without breaking every committed file in every downstream project.

Glyph normalization (see [grammar.md](grammar.md) "Unicode operator glyphs") is also edition-controlled — default is likely ASCII for searchability, with a project-level option to flip to glyphs.

## Performance posture

Format-on-save needs to be fast: milliseconds, not tens of milliseconds. The CST walk is O(n); the printer is O(n); no re-parsing inside the formatter. The frontend already serves both the CLI and the LSP from the same pure passes (see [lsp.md](lsp.md)), so the formatter inherits the same discipline — `fn(source) -> (formatted, Vec<Diagnostic>)`, no globals, no I/O.

## Out of scope for v1

Auto-import sorting, comment reflow at line-width limits, configurable rules beyond `indent_width` and `format.edition`, format-only-the-diff (`coddl fmt --check` is in scope; rustfmt-style range-only formatting in the LSP can land later). Add these once the rules above stabilize and the idempotency tests stick.
