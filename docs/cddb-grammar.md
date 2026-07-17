# Coddl catalog grammar (`.cddb`)

This document is the authoritative spec for the `.cddb` dialect — the
database catalog source family member. A `.cddb` file declares the
*conceptual schema* of one database: the truth about its logical shape,
independent of any consuming application's view.

For the overall four-file architecture (`.cd` / `.cddb` / `.cdmap` /
`.cdstore`), see [plan.md](plan.md). Lexical productions, comments,
whitespace, identifiers, literals, and punctuation are **shared with
`.cd`** — see `docs/grammar.md`. This document covers only the
syntactic productions specific to `.cddb`.

**Last sync:** unreleased — Phase 14 + relvar-init (`<Name> := <expr>;`).
Every commit that adds, removes, or changes a production or diagnostic
code in `parser_cddb.rs` updates this file in the same commit.


## Notation

Same EBNF dialect as `docs/grammar.md`.


## Syntactic productions

A `.cddb` document begins with a required `database <Name>;` header
followed by zero or more catalog items: base relvars (persistent
catalog state), virtual relvars (views), and base-relvar INIT values
(the initial value seeded at `coddl provision`).

```
<cddb-root>            ::= [ <database-decl> ] { <cddb-item> } ;        -- parse_cddb_root
                           -- The header is required for any non-empty
                           -- document. An empty file is well-formed.

<database-decl>        ::= 'database' <identifier> ';' ;                -- parse_database_decl

<cddb-item>            ::= <base-relvar-decl>
                         | <virtual-relvar-decl>
                         | <relvar-init>
                         | <public-relvar-decl>
                         | <private-relvar-decl>
                         | <unknown-item> ;

<base-relvar-decl>     ::= 'base' 'relvar' <identifier>
                           <heading>
                           { <key-clause> }
                           ';' ;                                        -- parse_base_relvar_decl
                           -- Multi-key declarations (`key {a} key {b}`)
                           -- parse; the typechecker stores each key
                           -- and uses the first for v1.

<virtual-relvar-decl>  ::= 'virtual' 'relvar' <identifier>
                           '=' <unknown-body> ';' ;                     -- parse_virtual_relvar_decl
                           -- v1 parses the keyword + name + `=` and
                           -- treats the RHS as an unknown body
                           -- recovered at the next top-level `;`. The
                           -- actual relational-expression grammar
                           -- lands in Phase 16.

<relvar-init>          ::= <identifier> ':=' <expr> ';' ;                -- parse_relvar_init
                           -- A base-relvar INIT value (the TTM initial
                           -- value applied at `coddl provision`). Keyed
                           -- off a leading identifier followed by `:=`
                           -- (checked before the keyword items so `:=`
                           -- disambiguates). The LHS names an existing
                           -- base relvar; the RHS is parsed as a general
                           -- <expr> (parser-permissive). The typechecker
                           -- resolves the LHS to a base relvar and
                           -- requires the RHS to be a ground relation
                           -- literal.

-- `public relvar` and `private relvar` parse here via the shared
-- `parse_public_relvar_decl` / `parse_private_relvar_decl` (see
-- docs/grammar.md); the typechecker emits T0014 because those
-- kinds belong in `.cd`. This is parser symmetry with the `.cd`
-- side, which similarly accepts `base` / `virtual`.
```

The shared `<heading>`, `<key-clause>`, `<expr>`, and `<unknown-item>`
productions are documented in `docs/grammar.md`. `<unknown-body>`
denotes "all tokens up to the next top-level `;` at bracket-depth
zero" — the same recovery shape as `<unknown-item>`.

`database`, `base`, `relvar`, `virtual`, `key` are **contextual
keywords**, recognized only in their respective positions. The five
reserved words and the word-operator glyphs (see `grammar.md`
"Reserved words") are rejected as declared names here too: the
database and relvar names emit this dialect's **PB0012**; relvar
*attributes* funnel through the shared heading parser and emit the
core **P0096**. Both are soft — the name still binds and parsing
continues.


## Parser diagnostics

| Code   | Trigger                                                |
|--------|--------------------------------------------------------|
| PB0001 | Expected `database <Name>;` header                     |
| PB0002 | Expected database name                                 |
| PB0003 | Expected `;` after `database <Name>`                   |
| PB0004 | Expected `base relvar` or `virtual relvar`             |
| PB0005 | Expected `relvar` after `base`                         |
| PB0006 | Expected relvar name (after `base relvar`)             |
| PB0007 | Expected `{` to start relvar heading                   |
| PB0008 | Expected `;` after `base relvar` declaration           |
| PB0009 | Expected `relvar` after `virtual`                      |
| PB0010 | Expected relvar name (after `virtual relvar`)          |
| PB0011 | Expected `=` after virtual relvar name                 |
| PB0012 | Reserved word used as an identifier (database or relvar name; soft — the name still binds). Relvar attributes emit the shared core's P0096 instead. |
| PB0013 | Expected an expression after `:=` in a relvar initializer      |
| PB0014 | Expected `;` after a relvar initializer                        |
