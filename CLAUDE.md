# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

Coddl is a compiler-in-design for a D-family relational language conforming to *The Third Manifesto* (Date & Darwen). Queries compile to SQL (SQLite first, Postgres later) against a pluggable backend; everything else compiles to LLVM IR and links against a Haskell runtime exposed via `foreign export ccall`.

**There is no source code yet.** The repo currently contains:
- `ARCHITECTURE.md` — the binding design document. Read this before suggesting *any* design change.
- `manifesto/` — plain-text mirror of *Databases, Types, and the Relational Model: The Third Manifesto* (3rd ed., 2014), one file per chapter/appendix, plus `INDEX.md`. The text is the spec Coddl conforms to.

No build commands, test commands, or lint commands exist yet — don't invent them. Cabal/Haskell tooling will land alongside the first packages (see ARCHITECTURE.md §8 for the planned layout).

## Working with the Manifesto reference

- Read `manifesto/INDEX.md` first; it maps each file to book pages and contents.
- Prefer the `.txt` files over `DTATRM.pdf` — they're grep-friendly and preserve page footers for citation. Use the PDF only for figures/tables that text extraction garbles. Note: PDF page ≈ book page + 11; the Read tool caps PDF reads at 20 pages.
- Cite Manifesto rules by their formal name (e.g., "RM Pre 8", "RM Pro 4", "OO Pre 6", "VSS 4") and book page number when arguing a design point.

## Non-negotiable design rules (already settled)

These are decided. Don't relitigate; flag explicitly if a proposal would break one.

- **Third Manifesto conformance is binding.** All RM/OO Prescriptions and Proscriptions adopted. See ARCHITECTURE.md §3 for the full list and the VSS adoption schedule.
- **No nulls. Ever.** RM Pro 4 is absolute. The type system has no nullable-attribute facility; the SQL backend never emits `NULL`, `NULLABLE`, `IS NULL`, or outer joins. Missing-information is a user-level `Maybe[T]` ADT or a database-design choice. Do not propose nullable anything.
- **No duplicates, no ordinal-position semantics on attributes/tuples, no tuple-at-a-time on relvars, no pointer attributes** (RM Pro 1–3, 7; OO Pro 2). Iteration over relations goes only via `LOAD ARRAY ... ORDER (...)` then a counted `DO` loop.
- **Relations are fully first-class and lazy; scalars are strict.** A relation expression doesn't run until forced. Equality is observational (RM Pre 8) — heading + tuple set, regardless of how the relation was built. Don't propose pervasive Haskell-style thunked scalars.
- **Host language is Haskell.** `megaparsec` for parsing; LLVM IR via text emission + `llc`/`clang` (not `llvm-hs`); `hasql` (Postgres) and `direct-sqlite` (SQLite) behind a typeclass; runtime is a Haskell library linked via `foreign export ccall`. Settled — don't suggest a host-language switch unless explicitly asked.
- **RelIR = Algebra A core + sugar layer.** EXTEND/WHERE/SUMMARIZE desugar to JOIN via "operators-as-relations." Work at the A level when proposing IR nodes or rewrites.
- **Surface syntax: uniform named-argument prefix style** (the form Tutorial D's own authors propose in ch. 5, pp. 127–128). Default form is `OP { paramName expr, paramName expr }` with braces. Infix retained for `=`, `<`, `+`, etc.; parenthesized positional kept for `COUNT`, `SIN`, `IS_*`. Don't propose Tutorial D's prose syntax verbatim.

## Architecture sections worth knowing by number

When citing the design doc in a discussion, ARCHITECTURE.md is organized as:

- §1 host language stack
- §2 pipeline diagram
- §3 conformance to the Manifesto (RM/OO Pres+Pros, VSSs, syntactic divergence)
- §4 the two IRs (RelIR = Algebra A; ProcIR = SSA for LLVM)
- §5 storage abstraction + mandatory SQL emission rules table
- §6 runtime (`libcoddl_runtime`)
- §7 type system (possreps, selectors, THE_, type generators)
- §8 cabal project layout
- §9 execution model (LOAD iteration, multiple assignment, transactions, relation handles)
- §10 risks worth deciding early
- §11 first milestone

## Operational notes

- The `tags` file is a ctags index seeded by the user — no code is indexed yet.
- The default branch is `main`. There is currently one commit; the working tree generally has staged design-doc edits.
