# Coddl store grammar (`.cdstore`)

This document is the authoritative spec for the `.cdstore` dialect —
the conceptual → physical binding file. A `.cdstore` file declares
which backend a database runs on (SQLite, Postgres, …) and how each
base catalog relvar maps to a physical table and column set.

For the overall four-file architecture, see [plan.md](plan.md).
Lexical productions are shared with `.cd` — see [grammar.md](grammar.md).

**Last sync:** unreleased — Phase 14.


## Notation

Same EBNF dialect as `docs/grammar.md`.


## Syntactic productions

A `.cdstore` document begins with a required header naming the
database, followed by a `backend` declaration and zero-or-more
`relvar` bindings.

```
<cdstore-root>   ::= [ <cdstore-header> ] { <cdstore-item> } ;  -- parse_cdstore_root

<cdstore-header> ::= 'store' 'for' <identifier> ';' ;           -- parse_cdstore_header

<cdstore-item>   ::= <backend-decl>
                   | <relvar-binding>
                   | <unknown-item> ;

<backend-decl>   ::= 'backend' <identifier>
                     '{' [ <cdstore-field> commalist ] '}'
                     ';' ;                                       -- parse_backend_decl

<relvar-binding> ::= 'relvar' <identifier> ':' 'table' <string-literal>
                     '{' <relvar-binding-body> '}'
                     ';' ;                                       -- parse_relvar_binding

<relvar-binding-body> ::= [ ( <columns-block> | <cdstore-field> )
                            { ',' ( <columns-block> | <cdstore-field> ) }
                            [ ',' ] ] ;                          -- parse_relvar_binding_body
                          -- v1 expects exactly one <columns-block>;
                          -- additional <cdstore-field>s are tolerated
                          -- for forward-compatibility.

<columns-block>  ::= 'columns' ':'
                     '{' <field-list> '}' ;                      -- parse_columns_block

<field-list>     ::= [ <cdstore-field> commalist ] ;             -- parse_field_list
                     -- Shared between <backend-decl> body and
                     -- <columns-block> body.

<cdstore-field>  ::= <identifier> ':' <cdstore-value> ;          -- parse_cdstore_field

<cdstore-value>  ::= <string-literal>
                   | <identifier>
                   | <env-call> ;                                -- parse_cdstore_value

<env-call>       ::= 'env' '(' <string-literal>
                     [ ',' 'default' ':' <string-literal> ]
                     ')' ;                                       -- parse_env_call
```

`store`, `for`, `backend`, `relvar`, `table`, `columns`, `default`,
`env` are **contextual keywords**, recognized only in their respective
positions. Coddl has no hard-reserved words.

### Value grammar is narrow

`.cdstore` is declarative configuration, not a programming surface.
The value grammar admits exactly three forms — string literals (e.g.
`"greetings.sqlite"`), bare identifiers (e.g. backend kinds, modes),
and `env(...)` calls. Tuple literals, expressions, and computed values
are intentionally rejected — these would compose with the rest of the
language in ways that complicate the static reasoning we want
configuration to have.

### Lifetime split

Backend kind, table names, and column mappings are **structural** —
consumed by codegen and baked into the compiled binary. Field values
under operational keys (`file:`, `dsn:`, pool size, …) are
**operational** — late-bound from environment variables at startup
with the declared value as a fallback. The grammar doesn't enforce
this split; it's a runtime convention (see `.local/phases.md` Phase
21).


## Parser diagnostics

| Code   | Trigger                                                |
|--------|--------------------------------------------------------|
| PS0001 | Expected `store for <database>;` header                |
| PS0002 | Expected `for` after `store`                           |
| PS0003 | Expected database name                                 |
| PS0004 | Expected `;` after `store for <database>`              |
| PS0005 | Expected `backend` or `relvar` declaration             |
| PS0006 | Expected backend kind name                             |
| PS0007 | Expected `{` to start backend body                     |
| PS0008 | Expected `;` after backend declaration                 |
| PS0009 | Expected `}` to close backend body                     |
| PS0010 | Expected relvar name                                   |
| PS0011 | Expected `:` after relvar name                         |
| PS0012 | Expected `table` after `:`                             |
| PS0013 | Expected table-name string literal                     |
| PS0014 | Expected `{` to start relvar binding body              |
| PS0015 | Expected `;` after relvar binding                      |
| PS0016 | Expected `}` to close relvar binding body              |
| PS0017 | Expected relvar binding field                          |
| PS0018 | Expected `:` after `columns`                           |
| PS0019 | Expected `{` to start columns block                    |
| PS0020 | Expected `}` to close columns block                    |
| PS0021 | Expected field name                                    |
| PS0022 | Expected `:` after field name                          |
| PS0023 | Expected string literal, identifier, or `env(...)`     |
| PS0024 | Expected `(` after `env`                               |
| PS0025 | Expected env-var name string literal                   |
| PS0026 | Expected `default` after `,` in `env(...)`             |
| PS0027 | Expected `:` after `default`                           |
| PS0028 | Expected default-value string literal                  |
| PS0029 | Expected `)` to close `env(...)`                       |
