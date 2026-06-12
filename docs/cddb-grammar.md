# Coddl catalog grammar (`.cddb`)

This document is the authoritative spec for the `.cddb` dialect — the
database catalog source family member. A `.cddb` file declares the
*conceptual schema* of one database: the truth about its logical shape,
independent of any consuming application's view.

For the overall four-file architecture (`.cdl` / `.cddb` / `.cdmap` /
`.cdstore`), see `ARCHITECTURE.md` and the higher-level discussion in
`.local/phases.md` (Phase 14). Lexical productions, comments,
whitespace, identifiers, literals, and punctuation are **shared with
`.cdl`** — see `docs/grammar.md`. This document covers only the
syntactic productions specific to `.cddb`.

**Last sync:** unreleased — Phase 14. Every commit that adds, removes,
or changes a production or diagnostic code in `parser_cddb.rs` updates
this file in the same commit.


## Notation

Same EBNF dialect as `docs/grammar.md`.


## Syntactic productions

A `.cddb` document begins with a required `database <Name>;` header
followed by zero or more catalog items: base relvars (persistent
catalog state) and virtual relvars (views).

```
<cddb-root>            ::= [ <database-decl> ] { <cddb-item> } ;        -- parse_cddb_root
                           -- The header is required for any non-empty
                           -- document. An empty file is well-formed.

<database-decl>        ::= 'database' <identifier> ';' ;                -- parse_database_decl

<cddb-item>            ::= <base-relvar-decl>
                         | <virtual-relvar-decl>
                         | <unknown-item> ;

<base-relvar-decl>     ::= 'base' 'relvar' <identifier>
                           <heading>
                           [ <key-clause> ]
                           ';' ;                                        -- parse_base_relvar_decl

<virtual-relvar-decl>  ::= 'virtual' 'relvar' <identifier>
                           '=' <unknown-body> ';' ;                     -- parse_virtual_relvar_decl
                           -- v1 parses the keyword + name + `=` and
                           -- treats the RHS as an unknown body
                           -- recovered at the next top-level `;`. The
                           -- actual relational-expression grammar
                           -- lands in Phase 16.
```

The shared `<heading>`, `<key-clause>`, and `<unknown-item>`
productions are documented in `docs/grammar.md`. `<unknown-body>`
denotes "all tokens up to the next top-level `;` at bracket-depth
zero" — the same recovery shape as `<unknown-item>`.

`database`, `base`, `relvar`, `virtual`, `key` are **contextual
keywords**, recognized only in their respective positions. Coddl has
no hard-reserved words.


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
