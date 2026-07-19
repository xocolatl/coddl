# Coddl store grammar (`.cdstore`)

This document is the authoritative spec for the `.cdstore` dialect — the
storage-binding file. A `.cdstore` is **DML into `coddl::storage`**, the storage
meta-catalog: a bare sequence of statements that populate its builtin relvars
(`Backends`, `ConnEnv`, `ConnDefault`, …) to describe which backend a database
runs on and how each connection field is sourced. The compiler evaluates that
DML at compile time to build the storage relation values it later queries.

For the overall four-file architecture, see [plan.md](plan.md). For the storage
meta-catalog schema, see `crates/coddl-stdlib/modules/coddl/storage.cd` and
[storage.md](storage.md).

**Last sync:** unreleased.


## Notation

Same EBNF dialect as `docs/grammar.md`.


## Syntactic productions

A `.cdstore` document is a bare sequence of statements — no header, no
declarations. `use module coddl::storage;` is **implicit**: the dialect
auto-activates that module, so its relvars are in scope without an import.

```
<cdstore-root> ::= { <stmt> } ;                                -- parse_cdstore_root
```

There is no `.cdstore`-specific production. `<stmt>` is the shared statement
grammar defined in [grammar.md](grammar.md) — the same production `.cd` operator
bodies use — driven at file top level rather than inside an `oper` body. In
practice a `.cdstore` uses only the DML statements (`insert`, `:=`, `update`,
`delete`, `truncate`) against the `coddl::storage` relvars; the typechecker
resolves the targets and the compile-time evaluator applies them.

Because `.cdstore` reuses the shared statement and expression grammar, it also
reuses its lexical forms (string literals, `Relation { … }` / tuple literals,
the `union`/`minus` operators, `:=`) and its parse diagnostics — the `P####`
codes documented in [grammar.md](grammar.md). The parser defines no codes of its
own. Typechecking uses the shared `T####` codes (e.g. `T0033`/`T0034` for a bad
DML target/heading).


## Compile-time evaluation diagnostics (`SE####`)

The compiler evaluates a `.cdstore`'s DML at compile time into the
`coddl::storage` relation values (crate `coddl-store`). Evaluation errors leave
the catalog partial and use the `SE####` namespace:

| Code   | Trigger                                                                |
|--------|------------------------------------------------------------------------|
| SE0001 | A relational operator that isn't evaluated (only `union` / `minus` over storage relvars and `Relation { … }` literals) |
| SE0002 | A relation element that isn't a tuple literal, or a cell that isn't a constant scalar |
| SE0003 | A constant cell whose value doesn't exist (overflow, division by zero) |
| SE0004 | `delete` / `update` — not yet evaluated at compile time                |
| SE0005 | A non-DML statement (a `.cdstore` holds only DML into `coddl::storage`) |
| SE0006 | Two tuples share a relvar's candidate key but differ in a non-key attribute |
