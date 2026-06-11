# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

Coddl is a compiler-in-design for a relational language conforming to *The Third Manifesto* (Date & Darwen). Coddl is its own D — *not* Tutorial D. Queries compile to SQL (SQLite first, Postgres later) against a pluggable backend; everything else compiles via ProcIR (a backend-agnostic SSA IR; v1 emits LLVM IR text, Cranelift/WASM planned) and links against a Rust `staticlib` runtime exposed through C ABI.

**There is no source code yet.** The repo currently contains:
- `ARCHITECTURE.md` — the binding design document. Read this before suggesting *any* design change.
- `manifesto/` — plain-text mirror of *Databases, Types, and the Relational Model: The Third Manifesto* (3rd ed., 2014), one file per chapter/appendix, plus `INDEX.md`. The text is the spec Coddl conforms to.

No build commands, test commands, or lint commands exist yet — don't invent them. Cargo/Rust tooling will land alongside the first crates (see ARCHITECTURE.md §8 for the planned layout).

## Working with the Manifesto reference

- Read `manifesto/INDEX.md` first; it maps each file to book pages and contents.
- Prefer the `.txt` files over `DTATRM.pdf` — they're grep-friendly and preserve page footers for citation. Use the PDF only for figures/tables that text extraction garbles. Note: PDF page ≈ book page + 11; the Read tool caps PDF reads at 20 pages.
- Cite Manifesto rules by their formal name (e.g., "RM Pre 8", "RM Pro 4", "OO Pre 6", "VSS 4") and book page number when arguing a design point.

## Non-negotiable design rules (already settled)

These are decided. Don't relitigate; flag explicitly if a proposal would break one.

- **Coddl is its own D — not Tutorial D.** Tutorial D is the Manifesto's reference D, useful as prior art, not a spec we follow. Where TTM prescribes behavior, Coddl conforms. Where TTM is silent, design the answer to fit the core principles below; convergence with Tutorial D's specific choice is incidental, not a goal. Sanctioned design freedoms are enumerated in ARCHITECTURE.md §3 (host language, surface syntax, evaluation strategy, IR choice); flag any proposed new design freedom explicitly rather than slipping it in.
- **Performance and long-term planning are core principles.** Runtime cost is a first-class concern — features that force unavoidable overhead the user can't opt out of are rejected. IRs, type representations, and crate boundaries are designed so deferred features land without rewrites. Keep data structures wider than current need; semantic boundaries over expedient ones. See ARCHITECTURE.md intro for the four-principle list.
- **Third Manifesto conformance is binding.** All RM/OO Prescriptions and Proscriptions adopted. See ARCHITECTURE.md §3 for the full list and the VSS adoption schedule.
- **No nulls. Ever.** RM Pro 4 is absolute. The type system has no nullable-attribute facility; the SQL backend never emits `NULL`, `NULLABLE`, `IS NULL`, or outer joins. The canonical TTM answer for missing information is **vertical decomposition** — split the relvar so absence of a fact = absence of a tuple in a side relvar (ch. 7 exercise 7.9). User-defined sum-type scalars are *permitted* but not recommended; propose decomposition first. Do not propose nullable anything.
- **No duplicates, no ordinal-position semantics on attributes/tuples, no tuple-at-a-time on relvars, no pointer attributes** (RM Pro 1–3, 7; OO Pro 2). Iteration over relations goes only via `LOAD ARRAY ... ORDER (...)` then a counted `DO` loop.
- **Relations are fully first-class and lazy; scalars are strict.** A relation expression doesn't run until forced. Equality is observational (RM Pre 8) — heading + tuple set, regardless of how the relation was built. Don't propose pervasive thunked/lazy scalars — laziness is a relation-level choice, not an evaluation strategy for the whole language.
- **Host language is Rust.** `chumsky` for parsing; LLVM IR via text emission + `llc`/`clang` (not `llvm-sys`/`inkwell`); `rusqlite` (SQLite) and `postgres` (Postgres) behind a trait; runtime is a Rust `staticlib` exposing `extern "C"` symbols. Settled — don't suggest a host-language switch unless explicitly asked. ProcIR is backend-agnostic at the node level; Cranelift and direct WASM emission are planned codegen siblings (§4, §8).
- **RelIR = Algebra A core + sugar layer.** EXTEND/WHERE/SUMMARIZE desugar to JOIN via "operators-as-relations." Work at the A level when proposing IR nodes or rewrites.
- **Surface syntax: uniform named-argument prefix style** (the form Tutorial D's own authors propose in ch. 5, pp. 127–128, but never adopt — Coddl does, with a colon between name and value). Default form is `OP { paramName: expr, paramName: expr }` with braces. Infix retained for binary operators (symbolic *and* textual): `=`, `<>`, `<`, `>`, `<=`, `>=`, `+`, `-`, `*`, `/`, `join`, `times`, `intersect`, `union`, `minus`, `where`, `and`, `or`, `mod`. `times` and `intersect` are typed aliases of `join` with heading-disjoint and heading-identical checks; `union` and `minus` require identical headings. The five relational ops lower to Algebra A primitives (`AND` / `OR` / `AND NOT`) with the heading check enforced at the type level. Comparison operators `<=`/`>=`/`<`/`>` are polymorphic: scalar comparison on scalars, subset / superset on relations; identical headings required. No separate `subset` keyword — `<=` covers it. `where` is also infix, special-cased: right operand is a predicate; the parser injects the left operand's heading into the predicate's name-resolution scope; `where` sits at the bottom of the precedence ladder so `R where x = 1 and y > 0` parses without parentheses. The scope-injection rule reuses for any later predicate-bearing construct (`extend`, `summarize` aggregates, possrep constraints). `and`/`or` are `Boolean × Boolean → Boolean`; `mod` is `Integer × Integer → Integer` at multiplicative precedence. Parenthesized positional kept for monadic textual operators (`count`, `sin`, `is_*`, `not`). Everything else (n-ary, structured operands) is named-prefix with braces.
- **No reserved words.** Coddl has no hard-reserved identifiers — the lexer emits `IDENT` for every word; the parser recognizes specific identifiers as keywords in specific syntactic positions. This is what enables the prefix-only constraint on textual operators above. Don't propose hard-reserving anything; keep the user identifier space unfettered (real domains use `name`, `type`, `from`, `to`, `order`, `value`, `with`, `by`, `and` as attribute names). See ARCHITECTURE.md §3 "Reserved words: none".
- **Brackets vs braces encode ordering.** `{ ... }` is unordered (set-like) — named-arg lists, `Tuple`/`Relation` literals, headings, `oper` parameter lists; reordering preserves meaning. `[ ... ]` is ordered (sequence) — `Sequence T` literals, `oper` bodies, `load`/`order` specs; reordering changes meaning. Maps onto RM Pro 1 (relational data has no ordinal position) vs. procedural code (sequential). Don't conflate; don't use `[]` for tuples or `{}` for `Sequence` literals or statement bodies. See ARCHITECTURE.md §3 "Brackets vs braces encode ordering".
- **Identifier case is settled and case-sensitive.** Lowercase / snake_case for keywords, built-in operators, built-in constants (`true`/`false`/`reltrue`/`relfalse`), and user-named operators / variables / attributes / parameters. PascalCase for type names (built-in `Integer`/`Rational`/`Approximate`/`Text`/`Character`/`Binary`/`Byte`/`Boolean`/`Tuple`/`Relation`/`Sequence`; user-defined `Customer` etc.) and relvar names by convention. The language enforces case sensitivity and the canonical case of built-in identifiers; user code may diverge but the formatter will not normalize across cases. See ARCHITECTURE.md §3 "Identifier case".
- **Identifier shape rules.** UAX #31 Unicode (`XID_Start` + `XID_Continue`), NFKC-normalized. Leading single `_` (`_unused`) marks "unused-OK" — typechecker won't warn. Bare `_` is the wildcard / "don't care" pattern (reserved for pattern matching). **Leading `__` (double underscore) is reserved for compiler-internal use** — `__plan_42`, `__tmp_*`, `__coddl_runtime_*` — and rejected from user identifiers; this is the private namespace the desugarer/optimizer/runtime use to avoid ever shadowing user code. Internal `__` (e.g. `foo__bar`) is unaffected — the rule is leading-prefix only. See ARCHITECTURE.md §3 "Identifier shape".
- **Literals:** `Text` is double-quoted (`"hello"`); `Character` is single-quoted exactly one codepoint (`'a'`, `'\n'`, `'\u{1F600}'`); `true`/`false` for `Boolean`. Multi-codepoint `'ab'` and empty `''` are rejected at lex time. Numeric literals are split lexically into three types: `42` → `Integer` (also `0x..`/`0b..`/`0o..`/`0d..`), `42.0` → `Rational`, `42e0`/`4.2e1` → `Approximate`. Underscores between digits are decoration. The lexer picks one type from the literal's *form* without inference.
- **Methods via UFCS on `self` parameter.** Any `oper` whose heading has a parameter literally named `self` can be called as `x.method { ... }` — pure sugar for `method { self: x, ... }`. `self` is convention (not a reserved word); position in the heading is irrelevant. Dispatch is by static type of the receiver. `x.name` without braces is a possrep accessor; `x.name {}` with braces is a method call — the braces disambiguate. See ARCHITECTURE.md §3 "Method-style call syntax".
- **Unicode operator glyphs are synonyms, not separate operators.** `⋈`/`∪`/`∩`/`∖`/`≤`/`⊆`/`≥`/`⊇`/`⊂`/`⊃`/`≠` lex as exact synonyms for `join`/`union`/`intersect`/`minus`/`<=`/`>=`/`<`/`>`/`<>`. The lexer emits the same token either way. Glyphs are added incrementally as their operators are settled. See ARCHITECTURE.md §3 "Unicode operator glyphs".
- **Nullary relations are `reltrue` and `relfalse`,** not TTM's `TABLE_DEE`/`TABLE_DUM`. Naming choice — same semantics; the new names say what they mean (relational true / false; multiplicative identity / zero of the join semiring) without forcing the reader to remember which of "dee" and "dum" is which. See ARCHITECTURE.md §7 "Naming note".
- **The frontend serves both the CLI driver and the VSCode LSP.** Every AST/IR node carries `(file_id, byte_range)` spans from the first lexer token; every analysis pass is `fn(Input) -> (Output, Vec<Diagnostic>)` with no `panic!`/`eprintln!` for user-visible errors; the parser recovers from errors rather than bailing. These aren't LSP-conditional — retrofitting any of them is a project-wide refactor. See ARCHITECTURE.md §12.
- **`coddl-syntax` produces a CST, not a plain AST.** The formatter (`coddl fmt`) needs every byte preserved (whitespace, comments). The parser produces a lossless concrete syntax tree; the AST is a typed view derived from it that the typechecker and downstream passes consume. Same backing storage, two views — don't propose a lossy AST or a side-channel trivia stream. See ARCHITECTURE.md §13.

## Architecture sections worth knowing by number

When citing the design doc in a discussion, ARCHITECTURE.md is organized as:

- §1 host language stack
- §2 pipeline diagram
- §3 conformance to the Manifesto (RM/OO Pres+Pros, VSSs, syntactic divergence)
- §4 the two IRs (RelIR = Algebra A; ProcIR = SSA for LLVM)
- §5 storage abstraction + mandatory SQL emission rules table
- §6 runtime (`libcoddl_runtime`)
- §7 type system (possreps, selectors, THE_, type generators)
- §8 Cargo workspace layout
- §9 execution model (LOAD iteration, multiple assignment, transactions, relation handles)
- §10 risks worth deciding early
- §11 first milestone
- §12 editor tooling (LSP + VSCode extension)
- §13 code formatter (`coddl fmt`)
- §14 memory model (refcount + COW + persistent data + per-scope arenas; no GC, no borrow checker)

- **Memory model: refcount + copy-on-write + persistent data + per-scope arenas. No tracing GC, no Rust-style borrow tracking.** Value semantics everywhere in the **surface language** — no `&` / `&mut` / `Box` / `Rc` visible to the user. Mutable locals are stack-allocated and don't escape. "Mutation" of heap values (`mut xs = xs ++ [item]`) produces a new value, with COW when the runtime can prove single ownership. Closures capture by value. The model leans on TTM's value semantics (RM Pre 8, OO Pro 2) so neither GC nor borrow checking is needed. **These rules are working defaults, not commandments** — push back if a proposal conflicts and we'll resolve explicitly. See ARCHITECTURE.md §14.
- **Surface ≠ implementation.** The above is the language surface. The compiled output uses pointers, stack frames, heap allocations, refcount operations, escape analysis, move optimisation, refcount elision, SROA, monomorphisation, and small-value inlining — the standard playbook for production value-semantics compilers (Swift, OCaml, ML). Don't propose surface-level pointer types because "the runtime needs pointers" — the runtime already has them; the surface is the layer the user reasons about. See ARCHITECTURE.md §14 "Surface vs implementation: two layers".

## Operational notes

- The `tags` file is a ctags index seeded by the user — no code is indexed yet.
- The default branch is `main`. There is currently one commit; the working tree generally has staged design-doc edits.
