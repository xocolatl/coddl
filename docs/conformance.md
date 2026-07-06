# Conformance to *The Third Manifesto*

Coddl conforms to *The Third Manifesto* (Date & Darwen, 3rd ed., 2014). The RM/OO Prescriptions and Proscriptions listed below are binding on design choices throughout this project — see [principles.md](principles.md) "Conformance over convenience."

This doc has three jobs: (1) name the adopted Pre/Pros so a proposal can be checked against them, (2) note which Very Strong Suggestions (VSSs) Coddl commits to vs. defers, (3) enumerate the sanctioned design freedoms — the places TTM is silent and Coddl picks. Anything beyond the sanctioned-freedoms list is *not* an open design space; propose explicitly and add to the list rather than slipping a new freedom in.

## Sanctioned design freedoms

These are the only places TTM doesn't dictate the answer:

1. **Host language and runtime stack.** Rust + LLVM-text codegen + a C-ABI Rust runtime. See [workspace.md](workspace.md), [runtime.md](runtime.md), [codegen.md](codegen.md).
2. **Surface syntax.** Uniform named-argument prefix style — see [grammar.md](grammar.md). In the spirit of the form Tutorial D's authors propose (ch. 5, pp. 127–128) but never adopt.
3. **Evaluation strategy.** Lazy relations, strict scalars. TTM doesn't address evaluation; this is our choice. See [runtime.md](runtime.md) "Lazy semantics."
4. **Canonical RelIR.** Algebra A as the IR core, which the Manifesto's authors recommend for any industrial-strength D (Appendix A). See [relir.md](relir.md).

Anything beyond this list is *not* a sanctioned design freedom. Add to the list rather than slipping in a quiet exception.

## Adopted (RM/OO Prescriptions and Proscriptions — non-negotiable)

- **Scalar types** carry possreps with selectors and `THE_C` accessors; named types are disjoint; no implicit coercion (RM Pre 1–5).
- **`Tuple H` and `Relation H`** are type generators with structural identity by heading (RM Pre 6–7). Tuple/relation type equality is set-equality of `{name: type}` pairs.
- **No nulls. Ever.** Missing information is handled by **vertical decomposition**: split the relvar so the absence of a fact is the absence of a tuple in a side relvar, rather than a placeholder in an attribute. This is the canonical TTM answer — see ch. 7 RM Pro 4 and exercise 7.9. The type system *permits* a user-defined sum-type scalar (e.g., an `Optional` with `Some`/`None` possreps) since arbitrary user-defined scalars are allowed, but it isn't the recommended approach. The SQL backend must never emit `NULL` to represent a *missing* attribute value, never emit `NULLABLE` columns for that purpose, and never use outer joins (see [sqlemit.md](sqlemit.md)). One narrow, sanctioned exception: SQLite cannot store IEEE `NaN`, so it *encodes* the `Approximate` `NaN` **value** as SQL `NULL`. There, `NULL` is a physical byte-pattern for a present value (like `Character`→`INTEGER`), not a missing-information marker — so the backend does translate it faithfully (retrieval `NULL`→`NaN`; `NaN`→`NULL` on store) and does emit NULL-aware equality (`a = b OR (a IS NULL AND b IS NULL)`) for `Approximate` comparisons. This does not reintroduce nullable attributes: the relvar stays total, and `NULL` never denotes absence.
- **No duplicate tuples**, **no ordinal-position semantics** for attributes or tuples, **no composite attributes** (use `Tuple`-typed attributes instead), **no domain-check override**, **no internal-level constructs in source** (RM Pro 1, 2, 3, 6, 8, 9).
- **No tuple-at-a-time operations on relvars or relations** (RM Pro 7). Iteration over a relation is only available via the `load` construct (see [runtime.md](runtime.md)) which orders, materializes into a `Sequence`, then iterates the sequence — the iteration boundary forces a deliberate materialization.
- **First-class `Tuple` and `Relation` types**, including parameters, return values, attribute types (so relation-valued attributes are allowed) (RM Pre 6–7, 9–10, 13).
- **Compile-time type checking** (OO Pre 1). Type-constraint violations (a selector argument failing its `possrep`'s `constraint`) remain run-time.
- **Computational completeness** (OO Pre 3). Coddl is the whole language; no host required.
- **Explicit transactions, nested transactions** (OO Pre 4, 5). See [runtime.md](runtime.md).
- **Aggregate identity on empty sets** (OO Pre 6) — `sum`=0, `and`=`true`, `or`=`false`, etc.
- **Relvars are not domains; no pointer attributes** in database relvars (OO Pro 1, 2).
- **Observational equality** (RM Pre 8): two values are equal iff indistinguishable under every operator on their type.
- **Multiple assignment** with the Manifesto's stated semantics (RM Pre 21): expand sugar; fold duplicate targets via WITH; evaluate all RHSs; assign atomically; check database constraints at the end of the whole MA (not per individual assignment, not at COMMIT). See [runtime.md](runtime.md).
- **Database constraints checked at statement boundaries** (not deferred to COMMIT) (RM Pre 23).
- **The Assignment Principle for views** — an INSERT into a view must fail if the inserted tuple would not appear in the view's defining expression (RM Pre 21).
- **The catalog is itself a set of relvars** — metacircular, queryable by ordinary relational expressions (RM Pre 25).

## Adopted (RM Very Strong Suggestions worth committing to in v1)

- **System keys** (VSS 1): `DEFAULT` operator-invocation clauses, a relational `TAG` operator (window-function lowering: `ROW_NUMBER() OVER (PARTITION BY …)`), nonupdatable system-default attributes.
- **Candidate-key inference** (VSS 3), minimally: propagate FDs through project/equijoin/restrict and surface inferred keys to the catalog. Best-effort.
- **Transition constraints** (VSS 4): primed-relvar syntax (`S'`) in `CONSTRAINT` bodies; pre-image captured by the runtime over delta sets, not by SQL triggers. See [risks.md](risks.md) for the snapshot-mechanism decision.
- **Quota queries** (VSS 5): `RANK r BY (DESC attr AS rankcol)` desugaring at the parser, lowering to `RANK()`/`DENSE_RANK()` window functions.

## Not adopted (matching the current Manifesto edition)

- **Foreign-key shorthand** (former VSS 2) — the authors formally deleted this VSS in later editions, and Coddl follows suit. Users write the general subset-constraint form directly: `CONSTRAINT SP{S#} ⊆ S{S#}` (RM Pre 23). This is what FK shorthand desugared to anyway, and it sidesteps the positional-matching example the authors regretted.

## Deferred to a later milestone

- **Generalized transitive closure** (VSS 6) — depends on VSS 7. Ship plain `TCLOSE` first.
- **User-defined heading-polymorphic operators** (VSS 7). Design the type system so adding row/heading polymorphism later doesn't force a rewrite: keep headings first-class in the type representation, don't hardwire monomorphic dispatch.
- **Type inheritance** (OO Pre 2, IM Pres). Conditional in the Manifesto. Coddl omits inheritance in v1; if added, it conforms to Part IV of the Manifesto in full.

## Skipped

- **SQL migration** (VSS 8). Out of scope for v1. Influence on the design is limited to: keep the type system extensible enough to add a parallel `SQL_*` type family later, and keep built-in operator names addressable (don't hardwire `=` to one type).
