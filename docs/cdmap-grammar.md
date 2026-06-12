# Coddl map grammar (`.cdmap`)

This document is the authoritative spec for the `.cdmap` dialect — the
external → conceptual adapter file. A `.cdmap` file binds an
application's `public relvar`s (declared in a `.cd` program) to the
database's catalog relvars (declared in a `.cddb`) through a chain of
project / rename clauses. Today's parser accepts identity mappings
only; richer chains land in Phase 16.

For the overall four-file architecture, see `ARCHITECTURE.md` and
`.local/phases.md` (Phase 14). Lexical productions are shared with
`.cd` — see `docs/grammar.md`.

**Last sync:** unreleased — Phase 14.


## Notation

Same EBNF dialect as `docs/grammar.md`.


## Syntactic productions

A `.cdmap` document begins with a required header binding one program
to one database, followed by zero or more identity mapping entries.

```
<cdmap-root>    ::= [ <cdmap-header> ] { <cdmap-entry> } ;     -- parse_cdmap_root
                    -- The header is required for any non-empty
                    -- document. An empty file is well-formed.

<cdmap-header>  ::= 'map' <identifier> 'to' <identifier> ';' ; -- parse_cdmap_header
                    -- The first <identifier> is the program name
                    -- (matches `program <name>;` in the .cd); the
                    -- second is the database name (matches
                    -- `database <name>;` in the .cddb).

<cdmap-entry>   ::= <identifier> '=' <identifier> ';' ;        -- parse_cdmap_entry
                    -- LHS is the application's public-relvar name;
                    -- RHS is the catalog (base or virtual) relvar
                    -- name. v1 supports identity mapping only.
```

`map`, `to` are **contextual keywords**, recognized only in their
header positions. Coddl has no hard-reserved words.

The `=` here is a **definition** (in the spirit of `let x = expr` in
`.cd`), not an assignment. The `.cdmap` line establishes that the
external name is *defined as* the catalog expression on the right;
assignment (`:=`) is reserved for `.cd`'s relvar mutation operator.


## Deliberately not yet in the grammar

The following are decided in `ARCHITECTURE.md` but not yet wired into
the `.cdmap` parser. Listed here so the omission is explicit:

- **Project chain**: `<cdmap-entry>` may eventually be
  `<identifier> '=' <identifier> 'project' '{' …  '}' ';'`.
- **Rename chain**: similarly `… 'rename' '{' <renames> '}' ';'`.
- **Mixed chains**: `<identifier> = <name> project { … } rename { … } ;`.

`CDMAP_PROJECT_CLAUSE` and `CDMAP_RENAME_CLAUSE` SyntaxKind variants
are allocated for these but no production parses into them today.


## Parser diagnostics

| Code   | Trigger                                                |
|--------|--------------------------------------------------------|
| PM0001 | Expected `map <program> to <database>;` header         |
| PM0002 | Expected program name                                  |
| PM0003 | Expected `to` between program and database name        |
| PM0004 | Expected database name                                 |
| PM0005 | Expected `;` after `map` header                        |
| PM0006 | Expected map entry (`<name> = <catalog-name>;`)        |
| PM0007 | Expected `=` after map LHS name                        |
| PM0008 | Expected catalog name on map RHS                       |
| PM0009 | Expected `;` after map entry                           |
