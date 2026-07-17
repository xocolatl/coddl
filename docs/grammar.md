# Grammar — surface syntax

The authoritative spec for Coddl's surface syntax — the precise form of the language the parser currently accepts, plus the design rationale behind every choice that doesn't immediately follow from TTM (see [conformance.md](conformance.md)).

This doc has two parts: **rationale** (the design decisions — why prefix-named-args, why only five reserved words, why brackets-vs-braces, etc.) and the **productions** (the EBNF the parser implements, lexical and syntactic).

**Last sync:** `94dfa9f`. Every commit that adds, removes, or changes a production, token, or diagnostic code updates this file in the same commit; `tools/check-grammar.sh` enforces it from the hygiene gate.

---

# Design rationale

## Uniform named-argument prefix style

Tutorial D's own authors observe (ch. 5, "A Remark on Syntax", pp. 127–128) that Tutorial D's operator syntax "is not very consistent" — mixed prefix/infix, positional matching that "violates the spirit, if not the letter, of RM Proscription 1." They sketch a uniform style they prefer but stop short of adopting: prefix for everything, argument matching by name, braces for argument bundles:

```
CARTESIAN { Y 2.5, X 5.0 }     -- not CARTESIAN ( 5.0, 2.5 )
JOIN      { left R, right S }  -- name the slots
```

**Coddl takes this as its default, with one variation: a colon between name and value.** The authors' examples above are space-separated; Coddl uses `name: value`. So the same examples in Coddl are:

```
cartesian { Y: 2.5, X: 5.0 }
R join S                              -- join is infix, see below
```

Reason for the colon: it makes the name/value boundary unambiguous when values are themselves identifiers or call expressions (`{ left: R, right: S join T }` reads clearly; `{ left R, right S join T }` requires the reader to know `R`/`S`/`T` aren't named-arg names). The colon also matches the way the same shape is written when `{ ... }` appears in a value position as a tuple literal (`{x: 1, y: 2}`), so one separator works in both roles.

Three operator-shape categories, with deliberate exceptions:

### Infix for binary operators (symbolic *and* textual)

- **Symbolic**: `=`, `<>`, `<`, `>`, `<=`, `>=`, `+`, `-`, `*`, `/`. The comparison operators `<=` and `>=` are polymorphic: scalar comparison on scalars (as ever); **subset** and **superset** on relations (`R <= S` iff every tuple in `R` appears in `S`; `S >= R` iff `R <= S`). `<` and `>` give strict subset / superset analogously. Identical headings are required for the relation overload — checked at compile time. There's no separate `subset` keyword; `<=` covers it.
- **Textual relational**: `join`, `times`, `intersect`, `compose`, `union`, `minus`, `matching`, `not matching`, `where`, `when`, `otherwise`.
- **Textual logical**: `and`, `or` (infix, both `Boolean × Boolean → Boolean`) and `not` (prefix, `Boolean → Boolean`). Precedence ladder `or` (1) < `and` (2) < `not` < comparison (3): `not` is a prefix operator whose operand parses at comparison level, so `not a and b` reads as `(not a) and b` and `not a = b` as `not (a = b)`. `not` also has the Unicode glyph `¬` (see "Unicode operator glyphs").
- **Textual arithmetic**: `div` (truncating integer division, toward zero), and the planned `mod` (remainder). `div` is `Integer × Integer → Integer` and binds at multiplicative precedence, alongside `*` and `/`. **The symbolic `/` is *exact* division**: `Integer × Integer → Rational` — `7 / 2` is the rational `7/2`, whereas `7 div 2` is the integer `3`. (`div` is the recognized keyword; `mod` is documented here but not yet wired.)

Reason: the named-prefix form is clumsy for ubiquitous dyadic ops on identifier-unfriendly names, and the textual binary ops all have natural infix readings from math and SQL. No-reserved-words still holds — `join` is recognized contextually in expression position; it remains a valid identifier elsewhere.

**`times` and `intersect` are typed aliases of `join`; `compose` is a sibling with a different lowering.** `join`, `times`, and `intersect` all lower to the same `AND` node in Algebra A (see [relir.md](relir.md) — `AND` generalizes TIMES and INTERSECT); `compose` lowers to `AND` followed by `REMOVE` of the shared attributes. All four exist for intent-signaling and compile-time enforcement — every check below is static and zero-cost at runtime:

- **`join`** requires the two headings to **partially overlap** — share at least one attribute, but **not** be identical. With no overlap the result would be a Cartesian product the user almost never means by accident, so the typechecker rejects it and suggests `times`. With *identical* headings the result would be a set intersection, so it rejects it and suggests `intersect`. The three AND-family operators therefore partition the heading relationship completely and exclusively: **disjoint → `times`, partial overlap → `join`, identical → `intersect`** — each operand pair has exactly one legal spelling.
- **`times`** requires the two headings to be **disjoint**. With any attribute in common the typechecker rejects it and suggests `join`.
- **`intersect`** requires the two headings to be **identical**. Anything else is rejected (it names the differing attributes).
- **`compose`** requires **partial overlap** — the same legal domain as `join`: it joins on the common attributes and removes them. It is meaningful only when both derived sets are non-empty: the shared attributes `A ∩ B` (the join/remove key) and the symmetric difference `A △ B` (the result heading). With no overlap (`A ∩ B` empty) the typechecker rejects it and suggests `times` (a disjoint compose is just a Cartesian product with nothing to remove). With identical headings (`A △ B` empty) every attribute would be removed, so the result is always the nullary relation regardless of the data — the typechecker rejects it and suggests `intersect`. A proper subset/superset is fine (it's partial overlap): `{a,b,c} compose {b,c}` joins on `{b,c}`, removes them, and keeps `{a}`.

**`union` and `minus` require identical headings.** `union` lowers to A-core `OR` (heading-agnostic relational union, restricted at the type level to matching headings since Coddl has no nulls). `minus` lowers to `AND NOT` (set difference is `R join (NOT S)` when headings match). Both checks are static; mismatched-heading attempts are rejected at compile time with a diagnostic.

**`matching` (semijoin) and `not matching` (antijoin) filter the *left* operand by existence in the right.** `R matching S` keeps the `R`-tuples that have a match in `S` on the shared attributes (`(R join S)` projected back onto `R`'s heading — TTM's SEMIJOIN); `R not matching S` keeps the `R`-tuples with **no** match (TTM's SEMIMINUS, `R minus (R matching S)`). The result heading is always `R`'s. Both take the **same legal domain as `join`/`compose` — partial overlap**: they match on the shared attributes, so identical headings are rejected and suggest the set operator they collapse to (`matching` → `intersect`, `not matching` → `minus`; T0094), and disjoint headings are rejected as a degenerate existence-guard with no key to match on (T0095); a shared-attribute type clash is T0036. They lower to the RelIR `Semijoin` sugar node, which the SQL emitter pushes as a correlated `WHERE [NOT] EXISTS` and the in-process path expands to join+project(+minus) (see [relir.md](relir.md), [sqlemit.md](sqlemit.md)). `matching`/`not matching` also have the Unicode glyph synonyms `⋉`/`▷` (see "Unicode operator glyphs"); `▷` is a single token, giving the two-word `not matching` a one-token spelling.

**`where` (restriction) is also infix and special-cased in two ways.** The right operand is a *predicate*, not another relation, and that has two consequences:

- **Scope injection.** Identifiers in the predicate resolve against the left operand's heading first, the enclosing scope second. `SP where s# = supplier` reads as: `s#` is the `SP` attribute; `supplier` is a parameter from the enclosing `oper`. The parser and typechecker inject the left operand's heading into the predicate's name-resolution scope automatically. This is the first construct with a non-uniform scoping rule; every later construct that takes a predicate (`extend`, `summarize`'s aggregate expressions, possrep constraints) reuses the same machinery.
- **Precedence.** `where` binds looser than `=`, `<`, `+`, `and`, `or` — the predicate is expected to be a full scalar expression. `R where x = 1 and y > 0` parses as `R where ((x = 1) and (y > 0))` without parentheses. Practically `where` sits at the bottom of the infix precedence ladder, alongside `union`/`minus`. Full precedence table lands when the parser does — exact order is deferred until then.

**`when` (gate) and `otherwise` (relational COALESCE) are the relational-control pair.** Both are infix at pipeline precedence (prec 0, with `where`), left-associative, pure sugar over the existing algebra — no new primitives:

- **`R when c`** ≡ `R times ⟨c⟩` — gates a **whole relation** by a scalar Boolean: `R` when `c` holds, the empty relation with `R`'s heading when it doesn't (the condition lifts to reltrue/relfalse — the join family's 1 and 0 — in the IR only, never at the surface). The deliberate contract with its sibling: **`where` filters per-tuple with the heading in scope; `when` gates the whole relation with a condition from the *enclosing* scope** — no heading injection, so an attribute name in a `when` condition is unresolved (T0099 hints at `where`). The condition is an ordinary strict scalar: it evaluates once, eagerly, even though relations are lazy. Chaining is AND (`R when a when b` ≡ `R when (a and b)`, by `times` associativity). Typing: `Relation H × Boolean → Relation H`; a relation-typed condition is rejected with a `times` suggestion (gating by a relation is what `times` already does). This is **not** a coercion in TTM's sense (ch. 3, p. 74 — coercion is *implicit* conversion of a wrongly-typed operand): the operand is required-Boolean and is-Boolean; a named operation "with operands that are explicitly defined to be of different types" is the Manifesto's own sanctioned crossing (the LOAD note, ch. 5, p. 123 — the same pattern Coddl's `load` already follows).
- **`R otherwise D`** ≡ `R union (D times (reltrue minus (R project {})))` — union-with-default: `R` if it is nonempty, else `D` (`R project {}` is the algebra's EXISTS; the arms are exclusive by construction, so nothing ever dedups). The relational COALESCE — the no-nulls answer to "if missing". Identical headings required, like `union` (T0038). `A when c otherwise D` reads left-to-right: `(A when c) otherwise D`.

Both remain ordinary identifiers everywhere else (`when` and `otherwise` are contextual — fine attribute or local names). Lowering: see [relir.md](relir.md) (the `Gate` restrict conjunct, the `Inst::Gate`/`Inst::Otherwise` in-process forms) and [sqlemit.md](sqlemit.md) (a relvar-rooted gate pushes as a `?N = 1` conjunct).

**`project` (projection) is a *postfix* operator, not infix.** `R project { a, b }` narrows the relation's heading to the named attributes — the right operand is a brace-list of bare attribute names (structurally identical to a `key { … }` clause), not another expression, so it doesn't fit the infix `<lhs> op <rhs>` shape. It is parsed as a postfix suffix at *pipeline precedence* — the same altitude as `where`, and gated to the top level so it binds to the whole pipeline rather than to a higher-precedence operand such as a `where` predicate. `R where p project { a }` reads as `(R where p) project { a }`; the reverse order `R project { a } where p` also parses, nesting left. It is the first postfix relational operator; later ones (`replace`, `extend`, `rename`, `tclose`, `wrap`, `unwrap`, `group`, `ungroup`, and future `summarize`) reuse the same pipeline slot. `project` remains a contextual keyword — a valid identifier everywhere else.

### Named-prefix with braces — the only call form

Every operator invocation is `name { … }` (or its dot-method sugar `R.method { … }`, see "Method-style call syntax"): selectors, `oper` calls, `extend`, `summarize`, `replace`, `group`, `ungroup`, `wrap`, `unwrap`, and so on. **There is no positional call form** — Coddl has no `f(x)` syntax; parentheses are for expression grouping only (see "Brackets vs braces encode ordering"). Arguments are named (`name: expr`); a brace may instead hold a bare list of attribute *names* where that is the operand shape (`project { a, b }`, `key { a, b }`). The binary relational operators — `join`, `times`, `intersect`, `compose`, `union`, `minus`, `matching`, `not matching`, `where`, `when`, `otherwise` — are **infix only**; there is no named-prefix brace variant for them.

This eliminates the relational-algebra/scalar-op syntactic distinction the authors regret, and matches RM Pro 1 (no ordinal-position semantics) at the surface where it's easiest to enforce.

## Brackets vs braces encode ordering

A consistent two-character distinction across the entire surface syntax:

- **`{ ... }` (curly braces) — unordered.** A set-like collection where position is meaningless. Used for named-argument lists, `Tuple` and `Relation` literals, heading declarations, and parameter lists in `oper` declarations. Reordering the contents preserves meaning.
- **`[ ... ]` (square brackets) — ordered.** A sequence where position is semantically significant. Used for `Sequence T` literals (`Sequence [1, 2, 3]`, `Sequence [tup1, tup2, tup3]` — the brackets always follow the `Sequence` generator keyword), operator bodies (statements run in order), `load` ordering specs, and any other context where the reader's expectation is "this is a sequence." Reordering changes meaning.
- **`( ... )` (parentheses)** — expression grouping only. There is no positional call form; every operator invocation uses `name { … }` (see "Named-prefix with braces" above).

This maps directly onto TTM: tuples, relations, and headings have no ordinal position semantics (RM Pro 1); they get `{ ... }`. Procedural code is sequential by nature; it gets `[ ... ]`. The punctuation tells the reader which kind of collection they're looking at without having to recall any context.

## Identifier case

Coddl is case-sensitive: `foo` and `Foo` are distinct identifiers. The language uses three case styles, applied consistently to built-ins and recommended for user code:

- **lowercase / snake_case** — keywords (`program`, `oper`, `where`, `join`, `load`, `if`, `then`, `else`, …), built-in operators (`and`, `or`, `join`, `union`, `extend`, …), built-in constants (`true`, `false`, `reltrue`, `relfalse`), and user-named operators, variables, attributes, and parameters.
- **PascalCase** — type names, both built-in (`Integer`, `Rational`, `Text`, `Character`, `Boolean`, `Tuple`, `Relation`, `Sequence`) and user-defined (`Customer`, `OrderLine`, `EmailAddress`); and relvar names by convention (`Customer`, `Suppliers`, `OrderLines`).

User code is not *required* to follow PascalCase for types and relvars — that's convention, not language. The language only enforces case sensitivity (so `customer` and `Customer` are different identifiers) and the canonical case of built-in identifiers (the `Integer` built-in is `Integer`, never `integer` or `INTEGER`).

## Identifier shape

- **Lexical class**: Unicode UAX #31 — `XID_Start` for the first character, `XID_Continue` for subsequent. The lexer NFKC-normalizes identifiers before comparison so visually equivalent character sequences denote the same identifier (e.g. `é` precomposed = `e` + combining acute).
- **Leading single underscore** (`_foo`) marks an identifier the developer is OK with being unused — the typechecker won't warn about unused locals or parameters whose name starts with `_`. Same convention as Rust.
- **Bare `_`** is the wildcard / "don't care" pattern. Reserved as a single-character form for (planned) pattern matching's catch-all branch.
- **Leading `__` (double underscore) is reserved for compiler-internal use** and rejected from user identifiers. This gives the desugarer, optimizer, and runtime a private namespace (`__plan_42`, `__tmp_join_lhs`, `__coddl_runtime_call`) that cannot ever shadow user code. snake_case with internal underscores (`foo_bar`, `write_line`, `_unused`) is unaffected — the rule is purely a leading-prefix check.

## Reserved words

At the lexer level there is no `KEYWORD` token type — every alphanumeric/underscore/`#` token is an `IDENT` — and the parser recognizes specific identifiers as keywords in specific syntactic positions. That much is unchanged. The honest statement of what it buys is a **taxonomy**, published in full below (the tables render `crates/coddl-syntax/src/keywords.rs`, the single source of truth both the parser's operator table and the AST's operator resolution consume; `tools/check-grammar.sh` Check 3 diffs this section against that file **bidirectionally**, and the pre-commit hook runs it):

- **Tier 1 — reserved (five words + the operator glyphs).** `true`, `false`, `if`, `not`, `extract` are claimed at an expression head with nothing to narrow on: `true`/`false` are the bare word, and `if`/`not`/`extract` are followed by an arbitrary expression, so no lookahead can ever free them — a binding under one of these names would be silently unreachable (`let true = 1; let x = true;` binds `x` to the Boolean literal) or misparse at every bare reference, and there is no quoting escape hatch (no Coddl analogue of SQL's `"order"`). So a declaration naming one is **rejected at the declaration site with P0096** — softly, on the E0007 model: the diagnostic is emitted, the name still binds, and parsing continues (LSP discipline; the trap is loud, not fatal). The check fires at every `.cd` name-declaring position — bindings, params and every heading attribute, oper/type/relvar names, loop and `load` binders, module-path segments, the file header, `database` bindings — and at every attribute-*creating* position: tuple/relation literal fields, `extend`/`replace`/`rename` new names, `wrap`/`group` new names. Reference positions (call-argument names, `update` clauses, `project` lists, dot access) are deliberately unchecked: a reference either resolves against an already-checked declaration or fails in the typechecker. The seven word-operator glyphs (`¬ ⋈ ∪ ∩ ∖ ⋉ ▷`) lex as `IDENT` and are rejected the same way. The `.cddb` parser's own decl sites (database and relvar names) emit its namespace sibling **PB0012**; `.cddb` relvar *attributes* funnel through the shared heading parser and emit P0096.
- **Tier 2 — positional claims narrowed by lookahead.** Every Tier-2 word is claimed only together with one token of lookahead, so the bare word is an ordinary identifier. The eleven statement heads are claimed at a statement boundary only when the next token is not `:=` — a variable named after any head stays assignable (`var delete := 1; delete := 2;` parses as a declaration and an ordinary assignment). The three expression heads `Relation` / `Sequence` / `transaction` are claimed only together with their delimiter (`{` / `[` / `[`) — a bare reference to a same-named binding is a NAME_REF, so a relvar named `Sequence` is queryable and an attribute named `transaction` usable. The same pattern frees `asc`/`desc` (recognized only when followed by another IDENT) and `builtin` (two-token `builtin relvar` vs `builtin oper`).
- **Tier 3 — vacuous claims (genuinely free).** Everything else is recognized in a position where a bare identifier is never a legal continuation — infix/postfix operator position, clause position after an introducing construct, item-head position, type position — so the claim shadows nothing. These are the words that keep the identifier space unfettered for real domains (`name`, `type`, `from`, `to`, `order`, `value`, `key`, `and` as attribute names all work).
- **Tier 4 — not keywords at all.** `reltrue` / `relfalse` are **not** parser-recognized: they are module-level `let`s in `coddl::core` (always in scope, resolved *after* everything else, deliberately user-shadowable — tests pin it). This is the model: vocabulary lives in the registry/stdlib and stays shadowable; the parser claims a word only when its *grammar* is special. The reserved set grows with syntax, never with library.

### Tier 1 — reserved

| Word | Glyph | Position | Notes |
|---|---|---|---|
| `true` | — | expression head (Boolean literal) | bare-word claim; a same-named binding could never be referenced — declaring one is P0096 |
| `false` | — | expression head (Boolean literal) | same |
| `if` | — | expression head (`if … then … else`) | followed by an arbitrary expression — unnarrowable; P0096 at declaration |
| `not` | `¬` | prefix operator; first token of `not matching` | unnarrowable; P0096 at declaration |
| `extract` | — | prefix operator (relation → tuple) | unnarrowable; P0096 at declaration |

The glyphs `⋈ ∪ ∩ ∖ ⋉ ▷` (operator rows below) and `¬` belong to this tier: they lex as `IDENT` with the glyph text, so declaring one is rejected exactly like the five words. The symbolic glyphs `≤ ≥ ≠ ⊂ ⊃ ⊆ ⊇` lex as operator *tokens*, never as identifiers — no declaration-site exposure.

### Tier 2 — positional claims, narrowed by lookahead

The bare word is an ordinary identifier in every case:

| Word | Position | Lookahead rule | What it frees |
|---|---|---|---|
| `let` `var` `truncate` `delete` `insert` `update` `for` `while` `do` `load` `return` | statement head | claimed only when the next token ≠ `:=` | assignment to a same-named variable — `delete := 2;` reaches `<assign-stmt>` |
| `Relation` | expression head (relation literal); also a type generator | claimed only with `{` | a bare reference (a relvar/binding named `Relation`) is a NAME_REF |
| `Sequence` | expression head (sequence literal); also a type generator | claimed only with `[` | a relvar named `Sequence` is declarable and queryable (the bioinformatics case) |
| `transaction` | expression head | claimed only with `[` | an attribute/binding named `transaction` is usable in expressions (the finance case) |

Residual traps that survive the narrowing (documented, accepted): a variable named `transaction` or `Sequence` cannot be *indexed* — `transaction [i]` is the transaction-expression claim and `Sequence [i]` parses as a one-element sequence literal; a *bare-reference* expression statement of a statement-head-named variable (`while;` — useless anyway); prefix-calling an oper named after a narrowed word (`Relation { … }` is claimed — UFCS `x.Relation { … }` is the escape hatch). One diagnostic trade came with the statement-head narrowing: the missing-loop-variable typo `for := 0 to 2 do [ … ];` now parses as an assignment to a variable named `for` and diagnoses generically at the assignment tail (P0013 at `to`) rather than with the targeted P0062. The expression-head narrowing retired P0019/P0031/P0055 (a bare head is no longer a parse error — it is a name; an unresolved one fails in typecheck).

### Tier 3 — vacuous claims

**Infix operators** (operator position only; precedence on the shared ladder with the symbolic operators — `* / div` 5, `+ - ||` 4, comparisons 3):

| Word | Glyph | Prec | | Word | Glyph | Prec |
|---|---|---|---|---|---|---|
| `div` | — | 5 | | `union` | `∪` | 0 |
| `and` | — | 2 | | `minus` | `∖` | 0 |
| `or` | — | 1 | | `matching` | `⋉` | 0 |
| `where` | — | 0 | | `not matching` | `▷` | 0 |
| `join` | `⋈` | 0 | | `when` | — | 0 |
| `times` | — | 0 | | `otherwise` | — | 0 |
| `compose` | — | 0 | | `intersect` | `∩` | 0 |

There is **no `mod`** — `div` (truncating integer division) is the only textual arithmetic keyword.

**Postfix pipeline suffixes** (pipeline operator position only): `project` `replace` `tclose` `extend` `rename` `wrap` `unwrap` `group` `ungroup` — plus `all` / `but` inside `project`'s brace form.

**Clause words** (each recognized only after its introducing construct): `then` `else` (if-expression) · `in` `to` (for-loop) · `from` `order` (load) · `asc` `desc` (sort item — already lookahead-narrowed: only when followed by another IDENT, so `[asc]` is an attribute) · `key` (relvar declaration).

**Item heads** (top level; no expression can start there): `program` `library` `module` `database` `public` `private` `base` `virtual` `builtin` `oper` `type` `use` `let` `var`. `builtin` is two-token narrowed (`builtin relvar` vs `builtin oper`); `relvar` and `key` are decl-interior words, not item heads.

**Type position** (`parse_type_ref`): `Relation` and `Sequence` as generators (their expression-head claims are the Tier-2 rows above). **`Tuple` is type-position only — it never claims expression space**; a variable, attribute, or oper named `Tuple` is fully usable. A *type* named after a generator is the one thing the claim forecloses — `type Relation { … }` would be unreachable — so the typechecker rejects it (T0085, same as the builtins).

**Sidecar dialects** (separate parsers, separate diagnostic namespaces): `.cddb` — `database` `base` `virtual` `public` `private` `relvar` `key`; `.cdstore` — `store` `for` `backend` `relvar` `table` `columns` `env` `default`; `.cdmap` — `map` `to`.

### Tier 4 — library vocabulary, not keywords

`reltrue` / `relfalse` are module-level `let`s in `coddl::core`, absorbed into every program's scope and resolved **last** in name lookup — shadowed by any user binding of the same name (this is tested behavior, including shadowing with a different type). Keep this as the model for growing the language's vocabulary: nothing about a library name is special to the parser.

The ergonomic goal stands: users model real domains with attribute names like `name`, `type`, `from`, `to`, `order`, `value`, `with`, `by`, `and` — every one of those is Tier 3 or entirely unclaimed, and hard-reserving them remains a tax Coddl refuses to pay. The cost is the prefix-only constraint on textual operators noted above, plus the five Tier-1 words whose expression-head grammar genuinely cannot share.

The TextMate grammar still pattern-highlights keywords at the lexical level (highlighting is a UX hint, not a lex check); the [LSP](lsp.md)'s semantic tokens correct mis-highlightings later where a user has used such a word as an identifier. Its keyword inventory is held to the taxonomy by the same Check 3 (bidirectionally: no fictional words highlighted, no `.cd` keyword unhighlighted); `reltrue`/`relfalse` and the builtin scalar type names are highlighted as *vocabulary*, outside the keyword cross-check.

## Unicode operator glyphs

A small set of single-codepoint mathematical glyphs are **exact synonyms** for their ASCII / keyword counterparts, so `R ⋈ S` and `R join S` are interchangeable in source. There are two lexing mechanisms behind the one meaning:

- **Symbolic glyphs** (`≤ ⊆ ≥ ⊇ ⊂ ⊃ ≠`) lex directly to the same *token kind* as their ASCII spelling (`≤` → `LtEq`), so the parser matches them with no glyph-specific logic.
- **Word glyphs** (`⋈ ∪ ∩ ∖ ¬ ⋉ ▷`) lex as an `IDENT` that keeps the glyph in the CST (formatter-friendly); the parser and AST then recognize the glyph text as a synonym at the *same recognition site* as the ASCII keyword — `⋈`/`∪`/`∩`/`∖` alongside `join`/`union`/`intersect`/`minus` in `peek_infix_prec` and `BinaryExpr::op_kind`, `¬` alongside `not`, and `⋉`/`▷` alongside `matching`/`not matching`. `▷` is a single token — it spells the two-word `not matching` in one glyph.

Grammar productions below name only the ASCII form.

| ASCII | Glyph(s) | Codepoint(s) |
|---|---|---|
| `join` | `⋈` | U+22C8 |
| `union` | `∪` | U+222A |
| `intersect` | `∩` | U+2229 |
| `minus` | `∖` | U+2216 SET MINUS (**not** U+005C reverse solidus — that's the string-escape character) |
| `matching` | `⋉` | U+22C9 LEFT NORMAL FACTOR SEMIDIRECT PRODUCT (the left-semijoin symbol) |
| `not matching` | `▷` | U+25B7 WHITE RIGHT-POINTING TRIANGLE (the antijoin symbol; one token) |
| `not` | `¬` | U+00AC NOT SIGN |
| `<=` | `≤`, `⊆` | U+2264, U+2286 |
| `>=` | `≥`, `⊇` | U+2265, U+2287 |
| `<` | `⊂` | U+2282 (relational strict-subset reading; scalar `<` keeps its ASCII form) |
| `>` | `⊃` | U+2283 |
| `<>` | `≠` | U+2260 |

Deliberately **not** in the synonym set: Greek letters (`π σ ρ γ` — too easily mistaken for ordinary identifiers in non-math source), Boolean truth-value glyphs (`⊤ ⊥`), and the empty-set glyph (`∅`). The [formatter](fmt.md) normalizes to one canonical form per `format.edition`.

## Literals

- **Text** literals: double-quoted, e.g. `"hello, world"`. Standard escape sequences `\n`, `\r`, `\t`, `\"`, `\\`, and `\u{HHHHHH}` for a Unicode codepoint (1–6 hex digits, value ≤ U+10FFFF, outside the UTF-16 surrogate range D800–DFFF). Multi-line is permitted — raw newlines are kept as-is.
- **Format-string** literals: a Text literal prefixed by `f` with the `f` **fused to the opening quote** (no space), e.g. `f"Hello, {name}!"`. Same body and escapes as a Text literal, plus `{name}` placeholders (a single attribute name) and `{{` / `}}` for literal braces. Its type is `FormatText` (see [typecheck.md](typecheck.md)), which appears only as the `template` argument of `format` — either inline or bound once to a `let` and reused (`let t = f"…"; format { template: t, … }`) — there is no `Text → FormatText` conversion, so a runtime `Text` can never be used as a template. Only the exact adjacency `f"` triggers it: a bare `f`, `f { … }`, `f "x"` (with a space), and `xf"x"` all stay an ordinary identifier (optionally) followed by a plain string. Lexical form → type, like the numeric shapes below. The lexer does **not** validate placeholders; that is a typecheck-time concern (T0055–T0059).
- **Character** literals: single-quoted, exactly one codepoint, e.g. `'a'`, `'\n'`, `'\u{1F600}'`. The lexer rejects empty `''` and multi-codepoint `'ab'` at the lexical level. Escape syntax matches Text.
- **Boolean** literals: `true`, `false`.
- **Numeric** literals — three lexical shapes, one per numeric type:
    - `42`, `0xff`, `0b1010`, `0o17`, `0d99` → **`Integer`** (base prefixes are case-insensitive; `0d` is the explicit-decimal prefix).
    - `42.0`, `3.14` → **`Rational`** (digits-dot-digits; both sides need at least one digit; no `42.` or `.5`).
    - `42e0`, `4.2e1`, `1e-9` → **`Approximate`** (exponent required; mantissa integer- *or* rational-shaped).

  Underscores between digits are decoration and stripped before conversion: `1_000_000`, `0xff_ff_ff`. The exponent marker `e` and the hex digits `a`–`f` are case-insensitive; the formatter normalizes to lowercase. The three-shape split is unambiguous: the lexer picks one of `Integer`/`Rational`/`Approximate` from the literal's *form* alone, without inference.

## Comments

- **Line comments** start with `//` and run to the end of the line.
- **Block comments** are delimited by `/*` and `*/` and **nest**. `/* outer /* inner */ still outer */` is one well-formed comment; the lexer counts depth on each `/*` and `*/`. The motivation is purely ergonomic — commenting out a region that already contains a block comment Just Works.

The lexer treats both kinds as trivia and attaches them to the CST per [fmt.md](fmt.md) so the formatter can preserve them. The choice of `//` over the `--` from Tutorial D / SQL is a deliberate move away from the SQL pedigree — `--` collides with the binary minus and "negative literal" patterns under enough lookahead pressure that committing to it would constrain unrelated grammar choices.

## Method-style call syntax (UFCS via `self`)

Any `oper` whose heading contains a parameter literally named `self` can be invoked as a method on that parameter's value:

```
oper to_codepoints { self: Text } : Sequence Character [
    /* ... */
];

let chars = textvar.to_codepoints {};
// same as:
let chars = to_codepoints { self: textvar };
```

Pure sugar — the surface form `x.f { ... }` desugars to `f { self: x, ... }` at parse time. Both spellings are accepted; the CST records the original so the formatter can preserve it.

`self` is a convention, not a reserved word. The method-call sugar fires only for headings whose parameter is literally named `self`; the slot's *position* in the heading is irrelevant (headings are unordered). There is no separate "method" declaration — methods are ordinary `oper`s with this one parameter-name convention.

**Dispatch is by static type of the receiver.** If two opers named `to_codepoints` exist with different `self` parameter types (`{ self: Text }` and `{ self: Bytes }`, say), the typechecker picks the one whose `self` type matches the receiver. Static overloading on `self` only — other parameters don't participate.

**Method call vs possrep accessor:**
- `x.possrep_name` (no braces) is the possrep accessor — returns the possrep view of `x`.
- `x.method_name { ... }` (with braces, including empty `{}`) is a method call.

The braces are the parser's disambiguation: `x.method` (no braces) is *always* an accessor; `x.method {}` is *always* a method call, even with zero arguments.

This mirrors UFCS in D / Nim and Rust's method syntax — but without an `impl` block: methods are just opers with a `self` parameter.

---

# Productions

The rest of this doc is the EBNF the parser implements.


## Notation

The grammar uses the EBNF dialect from
`/Users/vik/Projects/CoddLang/docs/appendix-a-grammar.ebnf`:

```
<nonterminal>   angle-bracketed nonterminals
'literal'       single-quoted terminals (default form)
"literal"       double-quoted (use when the literal contains an apostrophe)
[ X ]           X is optional (zero or one)
{ X }           X repeats zero or more times
{ X }+          X repeats one or more times
( X )           X is grouped (for applying a postfix shorthand like `commalist`)
X commalist     shorthand for X { ',' X } [ ',' ]
                — one or more, comma-separated, optional trailing comma
|               alternation
;               rule terminator
```


## Lexical productions

The lexer is a hand-rolled state machine; tokens are categorized into
identifiers, literals, punctuation, comparison operators, arithmetic
operators, and trivia. Whitespace and comments are emitted as
first-class tokens — the parser skips them; the CST keeps them.

### Comments and whitespace

```
<line-comment>  ::= '//' { <any-char-except-newline> } ;
<block-comment> ::= '/*' { <any-char> | <block-comment> } '*/' ;
<whitespace>    ::= { <White_Space-char> }+ ;
```

Block comments **nest** — a `/*` inside an open block opens an inner
level; `*/` closes one level; the comment ends when depth returns to
zero. Unterminated `/* … */` runs to end of input and emits diagnostic
**E0002**.

### Identifiers

```
<identifier>        ::= <identifier-start> { <identifier-continue> } ;
<identifier-start>  ::= -- any character with Unicode XID_Start, plus '_'
                        ;
<identifier-continue> ::= -- any character with Unicode XID_Continue,
                          -- plus '_'
                        ;
```

NFKC-normalized before comparison; case-sensitive. Leading single `_`
(`_unused`) is allowed and marks "unused-OK" semantically. **Leading
`__` is reserved** for compiler-internal names and emits **E0007**
when it appears in user source.

### Literals

```
<literal>             ::= <string-literal>
                        | <format-string-literal>
                        | <char-literal>
                        | <integer-literal>
                        | <rational-literal>
                        | <approximate-literal> ;

<string-literal>      ::= '"' { <string-char> } '"' ;
<format-string-literal> ::= 'f' '"' { <string-char> } '"' ;  -- 'f' fused to the quote, no space
<string-char>         ::= -- any source character other than '"' or '\'
                        | '\' <escape> ;
<escape>              ::= 'n' | 'r' | 't' | '"' | '\'
                        | 'u' '{' <hex-digit> { <hex-digit> } '}' ;

<char-literal>        ::= "'" ( <char-char> | '\' <escape> ) "'" ;
<char-char>           ::= -- any source character other than "'" or '\' ;

<integer-literal>     ::= <dec-digits>
                        | ( '0x' | '0X' ) <hex-digits>
                        | ( '0o' | '0O' ) <oct-digits>
                        | ( '0b' | '0B' ) <bin-digits>
                        | ( '0d' | '0D' ) <dec-digits> ;

<rational-literal>    ::= <dec-digits> '.' <dec-digits> ;

<approximate-literal> ::= ( <dec-digits> | <dec-digits> '.' <dec-digits> )
                          <exponent> ;
<exponent>            ::= ( 'e' | 'E' ) [ '+' | '-' ] <dec-digits> ;

<dec-digits>          ::= <dec-digit> { '_' <dec-digit> | <dec-digit> } ;
<bin-digits>          ::= <bin-digit> { '_' <bin-digit> | <bin-digit> } ;
<oct-digits>          ::= <oct-digit> { '_' <oct-digit> | <oct-digit> } ;
<hex-digits>          ::= <hex-digit> { '_' <hex-digit> | <hex-digit> } ;

<dec-digit>           ::= '0' | '1' | '2' | '3' | '4'
                        | '5' | '6' | '7' | '8' | '9' ;
<bin-digit>           ::= '0' | '1' ;
<oct-digit>           ::= '0' | '1' | '2' | '3' | '4' | '5' | '6' | '7' ;
<hex-digit>           ::= <dec-digit>
                        | 'a' | 'b' | 'c' | 'd' | 'e' | 'f'
                        | 'A' | 'B' | 'C' | 'D' | 'E' | 'F' ;
```

The three numeric *shapes* pick which of the three numeric trees the
value belongs to: `42` → `Integer`, `42.0` → `Rational`, `42e0` /
`4.2e1` → `Approximate`. The lexer never infers — the form decides.
Base prefixes (`0x`/`0b`/`0o`/`0d`) and the exponent marker `e` are
case-insensitive. Underscores between digits are decoration; the
parser strips them before conversion.

Empty `''` emits **E0004**. Multi-codepoint `'ab'` emits **E0006**.
Unterminated `'…` or `"…` emits **E0005** and **E0003** respectively.

### Punctuation and operator tokens

| Token         | Lexeme(s)                                       |
|---------------|-------------------------------------------------|
| `LBrace`      | `{`                                             |
| `RBrace`      | `}`                                             |
| `LBracket`    | `[`                                             |
| `RBracket`    | `]`                                             |
| `LParen`      | `(`                                             |
| `RParen`      | `)`                                             |
| `Semicolon`   | `;`                                             |
| `Comma`       | `,`                                             |
| `Colon`       | `:`                                             |
| `Dot`         | `.`                                             |
| `Assign`      | `:=`                                            |
| `Arrow`       | `->`                                            |
| `ColonColon`  | `::`  (module-path separator; `use` paths only) |
| `Eq`          | `=`                                             |
| `NotEq`       | `<>`, `≠`                                       |
| `Lt`          | `<`, `⊂`                                        |
| `Gt`          | `>`, `⊃`                                        |
| `LtEq`        | `<=`, `≤`, `⊆`                                  |
| `GtEq`        | `>=`, `≥`, `⊇`                                  |
| `Plus`        | `+`                                             |
| `Minus`       | `-`                                             |
| `Star`        | `*`                                             |
| `Slash`       | `/`                                             |
| `PipePipe`    | `\|\|`                                          |

### Unicode glyph synonyms

Single-codepoint mathematical glyphs lex as **exact synonyms** for
their ASCII / keyword counterparts. The lexer emits the same token
either way; grammar productions below name only the ASCII form.

| Glyph | Codepoint | Emits           | ASCII equivalent       |
|-------|-----------|-----------------|------------------------|
| `⋈`   | U+22C8    | `Ident("join")` | `join` (in expr position) |
| `∪`   | U+222A    | `Ident("union")`| `union`                |
| `∩`   | U+2229    | `Ident("intersect")` | `intersect`       |
| `∖`   | U+2216    | `Ident("minus")`| `minus`                |
| `⋉`   | U+22C9    | `Ident("⋉")`    | `matching` (semijoin)  |
| `▷`   | U+25B7    | `Ident("▷")`    | `not matching` (antijoin; one token) |
| `≤`   | U+2264    | `LtEq`          | `<=`                   |
| `⊆`   | U+2286    | `LtEq`          | `<=` (subset reading)  |
| `≥`   | U+2265    | `GtEq`          | `>=`                   |
| `⊇`   | U+2287    | `GtEq`          | `>=` (superset reading)|
| `⊂`   | U+2282    | `Lt`            | `<`                    |
| `⊃`   | U+2283    | `Gt`            | `>`                    |
| `≠`   | U+2260    | `NotEq`         | `<>`                   |

The CST keeps the original byte range so the formatter can preserve or
normalize per `format.edition`.

### Other lexer diagnostics

| Code  | Trigger                                              |
|-------|------------------------------------------------------|
| E0001 | Unexpected character at top level (anything the lexer doesn't recognize) |


## Syntactic productions

A `.cd` file opens with a mandatory file-kind header — `program`,
`library`, or `module` (see `<file-header>`) — followed by a top-level
sequence of declarations (`oper`, relvars, `type`, `use module`, …).
The header's presence, uniqueness, first-position, and the `program ⟺
oper main` rule are compilation-unit checks the plan layer enforces
(PL0012–PL0015), not the parser. Inside an operator body, the statement layer
recognizes expression statements only; the expression layer supports
identifier references, single-token literals, and brace-delimited call
expressions. Every rule below carries a comment naming the parser
function that implements it.

```
<root>          ::= { <item> } ;                              -- parse_root
<item>          ::= <file-header>
                  | <database-binding>
                  | <public-relvar-decl>
                  | <private-relvar-decl>
                  | <builtin-relvar-decl>
                  | <oper-decl>
                  | <type-decl>
                  | <use-decl>
                  | <let-stmt>
                  | <unknown-item> ;                          -- parse_item
                  -- A module-position `<let-stmt>` is a **constant binding**:
                  -- the same production as the statement form (name, optional
                  -- `: <type-ref>` annotation, `=` initializer), reused at
                  -- item level. The position carries the module-scope rules,
                  -- enforced by the typechecker, not the parser: the
                  -- initializer is mandatory and must be a constant
                  -- expression (T0022), bindings are order-independent like
                  -- every other item (reference cycles are T0097), and the
                  -- binding is evaluated once (scalars fold at compile time;
                  -- compound values materialize once at startup). A
                  -- module-position `var` parses for recovery and rejects
                  -- with P0086 — module-level mutable state is a relvar.

<file-header>   ::= ( 'program' | 'library' | 'module' )
                      <identifier> ';' ;                        -- parse_file_header
                  -- The mandatory file-kind header. `program` → executable
                  -- (requires an `oper main`); `library` → a stable-C-ABI
                  -- artifact a foreign host links (no `main`); `module` → an
                  -- intermediate unit linked into a consumer (no `main`). The
                  -- name is the bare leaf identity. Presence, uniqueness,
                  -- first-position, and the kind⟺main rule are compilation-unit
                  -- checks enforced in the plan layer (PL0012–PL0015), not the
                  -- parser.

<database-binding> ::= 'database' <identifier> ';' ;          -- parse_database_binding
                       -- Binds this program to a catalog. The compiler
                       -- discovers <name>.cddb and <name>.cdstore from
                       -- the declared name. Absent → program uses no
                       -- public relvars.

<public-relvar-decl>  ::= 'public' <relvar-with-heading> ;        -- parse_public_relvar_decl
                          -- Application-side relvar exposed to the catalog.

<private-relvar-decl> ::= 'private' <relvar-with-heading> ;       -- parse_private_relvar_decl
                          -- Application-side relvar internal to the program.

<builtin-relvar-decl> ::= 'builtin' <relvar-with-heading> ;      -- parse_builtin_relvar_decl
                          -- A compiler-provided relvar whose backing the
                          -- runtime supplies (the stdlib; e.g. `coddl::env`'s
                          -- `Environment`). Dispatched when `builtin` is
                          -- followed by `relvar` (vs `oper`). Registered from an
                          -- imported stdlib module by the typechecker; inert in
                          -- an ordinary checked file (like a user `builtin oper`).

<relvar-with-heading> ::= 'relvar' <identifier>
                          <heading>
                          { <key-clause> }
                          ';' ;                                -- parse_relvar_with_heading
                          -- Shared tail of the `public` / `private` / `builtin`
                          -- relvar productions. The kind keyword has
                          -- been consumed at the dispatch site.

-- `base relvar` and `virtual relvar` in `.cd` source parse via the
-- corresponding `parse_base_relvar_decl` / `parse_virtual_relvar_decl`
-- shared with the `.cddb` parser; the typechecker emits T0014 because
-- those kinds belong in `.cddb`. See docs/cddb-grammar.md for those
-- productions.

<oper-decl>     ::= [ 'builtin' ] 'oper' <identifier> <heading>
                    [ <return-clause> ]
                    [ <block> ] ';' ;                          -- parse_oper_decl
                    -- A plain `oper` requires a <block> body (P0006 if
                    -- absent). A leading `builtin` qualifier marks a
                    -- compiler-provided operator (the prelude — see
                    -- docs/prelude.md) that carries no body: a <block> after
                    -- it is P0078. `builtin` also qualifies a relvar
                    -- (<builtin-relvar-decl>), so item dispatch picks that on a
                    -- following `relvar`; `builtin` followed by neither `oper`
                    -- nor `relvar` is P0079. Mirrors the leading `public` /
                    -- `private` relvar qualifiers.
<return-clause> ::= '->' <type-ref> ;                          -- parse_return_clause

<type-decl>     ::= 'type' <identifier> ( '=' <type-ref> | <heading> ) ';' ; -- parse_type_decl
                    -- Two forms, chosen by the token after the name:
                    --   `= <type-ref>`  a transparent alias, naming a structural
                    --                   type (see docs/prelude.md).
                    --   `{ … }`         a possrep-scalar type — a distinct
                    --                   user-defined scalar whose possrep
                    --                   components are the <heading> (single-
                    --                   possrep tier; see docs/typecheck.md).
                    -- Dispatched on the leading contextual `type` keyword.
                    -- Missing name is P0080; a name followed by neither `{` nor
                    -- `=` is P0081; missing `;` is P0082. The checker rejects
                    -- shadowing a built-in type or type-generator name
                    -- (T0085) and a duplicate declaration (T0086).

<use-decl>      ::= 'use' 'module' <module-path> ';' ;         -- parse_use_decl
                    -- A module import. `module` is the only category today;
                    -- `use database …` is reserved for a later item form, so
                    -- the category word is spelled out rather than implied.
                    -- `use` and `module` are contextual keywords. Dispatched on
                    -- the leading contextual `use`. Missing `module` is P0083,
                    -- missing `;` is P0085. Imports bring the module's names
                    -- into scope UNQUALIFIED (bring-bare-names) — `::` is not
                    -- accepted in expression or type position. See
                    -- docs/prelude.md.

<module-path>   ::= <identifier> { '::' <identifier> } ;      -- parse_module_path
                    -- A `::`-separated module path (`coddl::core`). Wrapped in a
                    -- MODULE_PATH node. A missing segment — leading, or after a
                    -- `::` — is P0084.

<heading>       ::= '{' [ <param> commalist ] '}' ;            -- parse_heading
<param>         ::= <identifier> ':' <type-ref> ;              -- parse_param
<type-ref>      ::= 'Sequence' <type-ref>                       -- parse_type_ref
                  | ( 'Tuple' | 'Relation' ) <heading>
                  | <identifier> ;
                    -- Three generator-applied forms plus a leaf name.
                    -- `Sequence T` nests an element type-ref (e.g.
                    -- `Sequence Integer`, `Sequence Sequence Text`).
                    -- `Tuple <heading>` / `Relation <heading>` nest a
                    -- HEADING (e.g. `Relation { name: Text }`, `Tuple
                    -- {}`) whose attribute types are themselves type-refs
                    -- (so headings nest). Only when a `{` follows — a bare
                    -- `Tuple`/`Relation` stays a leaf name (resolving to
                    -- the unknown-type T0005). `Tuple`/`Relation` parameters
                    -- and returns are fully supported (the lowerer boxes large
                    -- tuples and tuple returns). `parse_heading` emits P0008 on
                    -- a missing `}`.

<key-clause>    ::= 'key' <ident-brace-list> ;                 -- parse_key_clause
<ident-brace-list> ::= '{' [ <identifier> commalist ] '}' ;    -- parse_ident_brace_list
                    -- Shared bare-identifier brace list (trailing
                    -- comma OK); used by <key-clause> and
                    -- <project-suffix>.
                    -- Candidate-key clause on a relvar declaration.
                    -- Shared between `.cd` application relvars
                    -- (`public` / `private`) and `.cddb` database
                    -- relvars (`base`). Multi-key declarations
                    -- (`key {a} key {b}`) parse; the typechecker
                    -- validates each key's attributes against the
                    -- heading, and downstream uses the first.

<block>         ::= '[' { <stmt> } [ <expr> ] ']' ;            -- parse_block
                    -- The optional trailing <expr> with no terminating
                    -- ';' is the block's tail expression; its value is
                    -- the block's value. Statements terminated by ';'
                    -- have their results discarded.
<stmt>          ::= <let-stmt>
                  | <var-stmt>
                  | <for-stmt>
                  | <while-stmt>
                  | <do-while-stmt>
                  | <load-stmt>
                  | <truncate-stmt>
                  | <delete-stmt>
                  | <insert-stmt>
                  | <update-stmt>
                  | <return-stmt>
                  | <assign-stmt>
                  | <expr> ';' ;                               -- parse_stmt (LET_STMT, VAR_STMT, TRUNCATE_STMT, DELETE_STMT, INSERT_STMT, UPDATE_STMT, ASSIGN_STMT, or EXPR_STMT)
                    -- A statement-head keyword is claimed only when the
                    -- next token is not `:=` (Parser::at_stmt_head), so a
                    -- variable named after any head stays assignable:
                    -- `delete := 2;` parses as an <assign-stmt>, not a
                    -- <delete-stmt>.
<return-stmt>   ::= 'return' [ <expr> ] ';' ;                   -- parse_return_stmt (RETURN_STMT)
                    -- Early return from the enclosing operator body. The value
                    -- is optional (`return;` yields Unit); a present value is
                    -- checked against the declared return type (T0018), and a
                    -- bare `return;` requires a Unit-returning oper. The value
                    -- is missing `;` recovers via P0013. `return` is a
                    -- contextual keyword recognized only as the leading token of
                    -- a statement (the `let` precedent). A `return` inside a
                    -- `transaction [...]` is rejected for now (T0093). Lowers to
                    -- a mid-function `Return` terminator that unwinds every
                    -- active scope; a `return`-terminated block or if-arm has
                    -- bottom type (see typecheck.md, `Never`).
<assign-stmt>   ::= <expr> ':=' <expr> ';' ;                   -- parse_stmt (ASSIGN_STMT)
                    -- Relational assignment. The parser accepts any
                    -- expression as the target (LHS); the typechecker
                    -- restricts it to a name bound to an assignable relvar
                    -- (public or private; T0033 otherwise) or a mutable
                    -- local. A public target is a write to its SQL table —
                    -- the RHS shape is recognized and emitted as surgical
                    -- DML at lowering. Statement-head-named targets reach
                    -- this production via the `:=` lookahead on the heads
                    -- (see <stmt> above).
<truncate-stmt> ::= 'truncate' <expr> ';' ;                    -- parse_truncate_stmt
                    -- Clear every tuple from a relvar — sugar for the
                    -- relational assignment `R := R minus R` (the surgical
                    -- whole-table delete shape). The operand is parsed as an
                    -- <expr> (P0014 if absent); the typechecker restricts it
                    -- to a bare assignable relvar name (T0033) and requires a
                    -- transaction for a public relvar (T0025). `truncate` is a
                    -- contextual keyword recognized only as the leading token
                    -- of a statement (the `let` precedent) — it stays a usable
                    -- identifier everywhere else.
<delete-stmt>   ::= 'delete' <expr> ';' ;                      -- parse_delete_stmt
                    -- Remove the matching tuples from a relvar — sugar for the
                    -- relational assignment `R := R minus (R where p)` (the
                    -- `DELETE … WHERE p` shape). The operand is parsed as an
                    -- <expr> (which consumes the `where`; P0014 if absent); the
                    -- typechecker requires a `where`-restriction over a bare
                    -- assignable relvar (T0033), the predicate *mandatory* — a
                    -- bare `delete R;` is T0052 (use `truncate`) — and a
                    -- transaction for a public relvar (T0025). `delete` is a
                    -- contextual keyword (the `let` precedent), usable as an
                    -- identifier elsewhere.
<insert-stmt>   ::= 'insert' <identifier> ( <tuple-set> | <expr> ) ';' ;
                                                               -- parse_insert_stmt
                    -- Add tuples to a relvar — sugar for the relational
                    -- assignment `R := R union <source>` (the idempotent
                    -- INSERT shape). After the target name, a `{` starts a
                    -- brace <tuple-set>; otherwise a relation <expr> source.
                    -- Missing target/source is P0014, missing `;` is P0013.
                    -- The typechecker requires a bare assignable relvar
                    -- (T0033), a transaction for a public relvar (T0025), and
                    -- the source heading to match the relvar's (T0034).
                    -- `insert` is a contextual keyword (the `let` precedent).
<tuple-set>     ::= '{' <relation-lit-body> ;                 -- parse_tuple_set
                    -- A brace tuple-set — the keyword-less spelling of a
                    -- relation literal. It builds the same RELATION_LIT node and
                    -- shares the <relation-lit-body> element-expression body, so
                    -- the checker and lowerer treat it as a relation source
                    -- uniformly (each element is a tuple-typed expression;
                    -- `insert R { req }` inserts the single tuple `req`). An empty
                    -- `{}` is `relfalse` (the nullary empty relation); inserting
                    -- it into a headed relvar is a heading mismatch (T0034).
<update-stmt>   ::= 'update' <expr> <arg-list> ';' ;           -- parse_update_stmt
                    -- Overwrite named attributes of the matching tuples — sugar
                    -- for `R := (R where ¬p) union ((R where p) «substitute»)`
                    -- (the `UPDATE … SET … WHERE p` shape), or a bare substitute
                    -- for update-all. The operand (`R` or `R where p`) is parsed
                    -- with brace-call **suppressed** so the trailing `{ … }` is
                    -- the update clause, not a `CALL_EXPR` on the operand (P0014
                    -- if the operand is absent, P0054 if the clause `{` is). The
                    -- clause is an <arg-list> (colon required, like `replace`).
                    -- A brace-call *inside* the predicate must be parenthesized
                    -- (`update R where (f { x: 1 }) > 0 { … }`) — parentheses
                    -- re-enable the brace-call. The typechecker requires a bare
                    -- assignable relvar (T0033), a transaction for a public
                    -- relvar (T0025), each target attribute to exist (T0053) with
                    -- a type-matching value (T0034); unlike `replace`, constant
                    -- and bare-reference values are allowed. `update` is a
                    -- contextual keyword (the `let` precedent).
<let-stmt>      ::= 'let' <identifier> [ ':' <type-ref> ]
                    [ '=' <expr> ] ';' ;                       -- parse_let_stmt
                    -- An **immutable** value binding: the operator is `=`. A
                    -- `:=` here is the `var` operator by mistake — P0067, then
                    -- the `:=` is consumed for recovery so the RHS still parses.
                    -- The initializer is optional at the parse level (`let x;`
                    -- parses), but an uninitialized `let` is a type error (T0078)
                    -- — an immutable binding must be initialized. `let` is a
                    -- contextual keyword (usable as an identifier elsewhere; no
                    -- reserved words).
<var-stmt>      ::= 'var' <identifier> [ ':' <type-ref> ]
                    [ ':=' <expr> ] ';' ;                      -- parse_var_stmt (VAR_STMT)
                    -- A **mutable** value binding — the reassignable sibling of
                    -- `let`. The operator is `:=`, matching the operator that
                    -- reassigns it (`<name> := <expr>;`, an <assign-stmt>) and
                    -- the counted-`for` counter init; `=` here is the `let`
                    -- operator by mistake — P0068, then `=` is consumed for
                    -- recovery. Both the type annotation and the initializer are
                    -- optional: a bare `var x;` declares an uninitialized mutable
                    -- local whose type is **inferred from its first assignment**
                    -- (definite-assignment, T0079, then ensures it is assigned
                    -- before it is read). The `:` annotation never eats the `:`
                    -- of `:=` (which lexes as one `ASSIGN` token). `var` is a
                    -- contextual keyword recognized only as the leading token of a
                    -- statement (the `let` precedent), usable as an identifier
                    -- elsewhere — with the usual cost that a bare `var := …;`
                    -- reassigning a relvar/local literally named `var` is instead
                    -- read as a (malformed) `var` declaration.
<for-stmt>      ::= 'for' <identifier>
                      ( ':=' <expr> 'to' <expr> | 'in' <expr> )
                      'do' <block> ';' ;                       -- parse_for_stmt (FOR_STMT)
                    -- Two forms, dispatched on the header separator after the
                    -- loop variable: `:=` → a **counted** loop with an INCLUSIVE
                    -- upper bound (`i <= hi`); `in` → an **element** loop over a
                    -- Sequence. `in`/`to`/`do` are contextual keywords recognized
                    -- only in this statement position (the `let` precedent); each
                    -- <expr> stops at the next keyword because none of them is an
                    -- infix operator or a postfix trigger. The loop variable is
                    -- loop-scoped and immutable — assigning it is T0072. Counted:
                    -- both bounds must be Integer (T0071); `lo > hi` runs zero
                    -- times (empty-safe at the header test). Element: the operand
                    -- must be a `Sequence T` (a relation is T0073, pointing at
                    -- `load … order`), the variable takes the element type `T`.
                    -- A trailing `;` is required — a `for` is a statement, never
                    -- a value. Both forms build one FOR_STMT; the AST tells them
                    -- apart by the `:=` token.
                    --
                    -- The element form is pure sugar, desugared in the lowerer
                    -- onto the counted loop: `for name in seq do [ … ]` becomes
                    -- `for __i := 0 to cardinality(seq) - 1 do [ let name =
                    -- seq[__i]; … ]` (`__i` in the compiler-internal namespace).
                    --
                    -- P0062 on a missing loop variable, P0063 on a missing
                    -- `:=`/`in`, P0064 on a missing `to` (counted), P0065 on a
                    -- missing `do`, P0066 on a missing body `[`; P0013 on a
                    -- missing trailing `;`.
<while-stmt>    ::= 'while' <expr> 'do' <block> ';' ;         -- parse_while_stmt (WHILE_STMT)
                    -- The **pre-test** loop: the condition (a full <expr> that
                    -- stops at `do`, which is neither an infix operator nor a
                    -- postfix trigger) is tested before each iteration, and the
                    -- loop runs while it is `true` (Boolean, T0080). Empty-safe.
                    -- `while` is the loop primitive — the counted/element `for`
                    -- forms desugar onto a counted loop, and `do … while` is this
                    -- loop with the test relocated after the body. There is no
                    -- loop variable; progress is the user's own `<name> := …` on a
                    -- `var` declared outside the loop (an always-`true` condition
                    -- is a legal infinite loop, not the compiler's concern).
                    -- `while`/`do` are contextual keywords recognized only in this
                    -- statement position (the `let` precedent). P0069 on a missing
                    -- `do`, P0070 on a missing body `[`; P0013 on a missing `;`.
<do-while-stmt> ::= 'do' <block> 'while' <expr> ';' ;         -- parse_do_while_stmt (DO_WHILE_STMT)
                    -- The **post-test** loop (C-style do…while): the body runs
                    -- **once before** the condition is first tested, then repeats
                    -- while the condition is `true` (Boolean, T0080). Because the
                    -- body always runs at least once, a `do [ … names[0] … ] while
                    -- …` over an empty sequence indexes out of bounds — a
                    -- documented caveat the user owns (`for … in` and `while` are
                    -- empty-safe; this form is not). A statement-leading `do` is
                    -- reserved exclusively for this form and *requires* a trailing
                    -- `while <cond>`: a bare `do [ … ];` block statement is a parse
                    -- error (P0072), otherwise `do [B] while c do [B2]` would be
                    -- ambiguous against "run block, then a pre-test loop". P0071 on
                    -- a missing body `[`, P0072 on a missing `while`; P0013 on a
                    -- missing `;`.
<load-stmt>     ::= 'load' <identifier> 'from' <expr>
                    [ 'order' '[' <sort-item> { ',' <sort-item> } ']' ] ';' ;  -- parse_load_stmt (LOAD_STMT)
                    -- The sole relation→sequence iteration gate (RM Pro 7):
                    -- force the source relation, impose an order, and materialize
                    -- its tuples into the `Sequence` target. The source is any
                    -- relation <expr>; it stops at `order`, which is neither an
                    -- infix operator nor a postfix trigger. The `order` clause is
                    -- an ORDERED bracket-list of <sort-item>s (sort precedence is
                    -- ordinal) and is optional — the reverse `load <relvar> from
                    -- <sequence>` form carries none; the SOURCE type disambiguates
                    -- direction at typecheck (a `Relation` source is the forward
                    -- form → ordered `Sequence` into a `var`; a `Sequence` source
                    -- is the reverse form → seal into a private relvar as a set).
                    -- `load` has no
                    -- projection slot: to keep only some attributes, project in
                    -- the source <expr> (`load n from (R project { a }) order
                    -- [asc a]`). `load`/`from`/`order` are contextual keywords
                    -- recognized only in this statement position (Coddl reserves
                    -- no words). P0073 on a missing target name, P0074 on a
                    -- missing `from` (a missing source expression is the uniform
                    -- P0014), P0075 on a missing order-list `[`, P0076 on an
                    -- unterminated order list, P0077 on an empty/missing order
                    -- key; P0013 on a missing `;`.
<sort-item>     ::= [ 'asc' | 'desc' ] <identifier> ;          -- parse_sort_item (SORT_ITEM)
                    -- One order key: an optional direction keyword (`asc`/`desc`,
                    -- contextual — recognized only when an attribute <identifier>
                    -- follows, so a bare attribute named `asc`/`desc` still
                    -- parses) and the order-key attribute name. A bare attribute
                    -- defaults to `asc`. Shared with the (planned) window `rank`
                    -- sort list. P0077 on a missing attribute.

<expr>          ::= <expr-prec> ;                            -- parse_expr
<expr-prec>     ::= <primary-expr> { <postfix> }
                    { <infix-op> <expr-prec> | <project-suffix>
                      | <replace-suffix> | <tclose-suffix>
                      | <extend-suffix> | <rename-suffix>
                      | <wrap-suffix> | <unwrap-suffix>
                      | <group-suffix> | <ungroup-suffix> } ;
                                                               -- parse_expr_prec
                    -- Pratt precedence ladder; left-associative.
                    -- min_prec drives which operators may be
                    -- consumed; the parser recurses with `prec + 1`
                    -- for each rhs. The <project-suffix> / <replace-suffix>
                    -- / <tclose-suffix> / <extend-suffix> / <rename-suffix> /
                    -- <wrap-suffix> / <unwrap-suffix> / <group-suffix> /
                    -- <ungroup-suffix> postfix forms are
                    -- consumed only at pipeline level (min_prec 0),
                    -- interleaved with infix ops, so they bind to the whole
                    -- pipeline.
<infix-op>      ::= 'where'                                    -- prec 0
                  | 'when'                                     -- prec 0
                  | 'otherwise'                                -- prec 0
                  | 'join'                                     -- prec 0
                  | 'times'                                    -- prec 0
                  | 'compose'                                  -- prec 0
                  | 'intersect'                                -- prec 0
                  | 'union'                                    -- prec 0
                  | 'minus'                                    -- prec 0
                  | 'matching'                                 -- prec 0
                  | 'not' 'matching'                           -- prec 0 (two tokens)
                  | 'or'                                       -- prec 1
                  | 'and'                                      -- prec 2
                  | '=' | '<>' | '<' | '>' | '<=' | '>='       -- prec 3
                  | '+' | '-' | '||'                           -- prec 4
                  | '*' | '/' ;                                -- prec 5
                    -- `where`, `when`, `otherwise`, `and`, `or`
                    -- are contextual keywords; the symbolic forms
                    -- are token kinds already lexed (Eq, Lt, Gt,
                    -- LtEq, GtEq, NotEq, Plus, Minus, Star, Slash,
                    -- PipePipe). Arithmetic binds tighter than
                    -- comparison: additive `+`/`-` and
                    -- concatenation `||` at prec 4, multiplicative
                    -- `*`/`/` at prec 5. `||` shares prec 4 with
                    -- `+`/`-`; its rank there is immaterial since
                    -- its operands (Text/Character) never mix with
                    -- arithmetic. The relational ops `join`/`times`/
                    -- `compose`/`intersect`/`union`/`minus`/
                    -- `matching`/`not matching` are also contextual
                    -- keywords. (Symbolic `-` is `Sub`; the keyword
                    -- `minus` is the relational set-difference op.)
                    -- `matching` (semijoin) is one token; `not
                    -- matching` (antijoin) is two — the parser peeks
                    -- `not`+`matching` and bumps both. `join`/
                    -- `union`/`intersect`/`minus`/`matching`/
                    -- `not matching` also accept their glyph
                    -- synonyms `⋈`/`∪`/`∩`/`∖`/`⋉`/`▷` (§ Unicode
                    -- operator glyphs; `▷` is a one-token `not
                    -- matching`); `times`/`compose`/`when`/
                    -- `otherwise` have none. `when` (gate) and
                    -- `otherwise` (relational COALESCE) sit at the
                    -- pipeline bottom with `where` — see
                    -- § Relational control: `when` and `otherwise`.
<postfix>       ::= <arg-list>                                 -- call: CALL_EXPR
                  | <field-access-tail>                        -- field access: FIELD_ACCESS
                  | <index-tail> ;                             -- index: INDEX_EXPR
<field-access-tail> ::= '.' <identifier> ;
                    -- A brace call over a field access — a CALL_EXPR whose
                    -- callee is a FIELD_ACCESS (`x.m { … }`) — is the UFCS
                    -- method-call form: sugar for `m { self: x, … }`,
                    -- resolved by the typechecker (dispatch on the receiver's
                    -- type; T0070 if `m` has no `self` param). No dedicated
                    -- production — it falls out of postfix chaining. A bare
                    -- `x.m` with no braces stays a possrep/tuple field access.
<index-tail>    ::= '[' <expr> ']' ;                          -- INDEX_EXPR
                    -- 0-based postfix sequence index `s[i]`,
                    -- parsed inline in the postfix loop (like
                    -- <field-access-tail>) so it binds tighter than
                    -- the pipeline suffixes and `x[0][1]` nests left.
                    -- P0058 on a missing index expr, P0057 on a
                    -- missing `]`. Typecheck: operand `Sequence T`,
                    -- index `Integer`, result `T` (T0065 / T0066).
<primary-expr>  ::= <name-ref>
                  | <literal>
                  | <bool-lit>
                  | <transaction-expr>
                  | <if-expr>
                  | <tuple-lit>
                  | <relation-lit>
                  | <sequence-lit>
                  | <extract-expr>
                  | <not-expr>
                  | <paren-expr> ;                             -- parse_primary_expr
<bool-lit>      ::= 'true' | 'false' ;                         -- BOOL_LITERAL
<extract-expr>  ::= 'extract' <expr-prec> ;                    -- parse_extract_expr
                    -- TTM RM Pre 10 cardinality-checked
                    -- relation-to-tuple primitive. Wraps in
                    -- UNARY_EXPR. The operand parses at the
                    -- lowest precedence so `extract R where p`
                    -- reads as `extract (R where p)` without
                    -- parens.
<not-expr>      ::= ( 'not' | '¬' ) <expr-prec> ;               -- parse_not_expr
                    -- Boolean prefix negation (`Boolean →
                    -- Boolean`). Wraps in UNARY_EXPR. The operand
                    -- parses at prec 3 (comparison level), so
                    -- comparison/arithmetic bind inside but
                    -- `and`/`or` stay outside: `not a and b` is
                    -- `(not a) and b`, `not a = b` is `not (a = b)`.
                    -- `¬` is the glyph synonym (lexed as an IDENT,
                    -- matched at the same recognition site). T0021
                    -- if the operand isn't Boolean.
<paren-expr>    ::= '(' <expr-prec> ')' ;                       -- PAREN_EXPR
                    -- Transparent grouping; AST view unwraps to
                    -- the inner expression so the typechecker /
                    -- lowerer never see the wrapper.
<project-suffix> ::= 'project' [ 'all' 'but' ] <ident-brace-list> ; -- parse_project_suffix
                    -- Relational projection. Postfix at pipeline
                    -- precedence; wraps the operand in PROJECT_EXPR.
                    -- Left-associative, and interleaves with `where`
                    -- in either order. Plain `project { … }` keeps the
                    -- named attributes; `project all but { … }` removes
                    -- them (keeps the complement). `all`/`but` are
                    -- contextual keywords, valid identifiers elsewhere.
                    -- See the projection rationale above.
<replace-suffix> ::= 'replace' <arg-list> ;                    -- parse_replace_suffix
                    -- Relational replace (compute-and-consume). Postfix at
                    -- pipeline precedence (like <project-suffix>); wraps the
                    -- operand in REPLACE_EXPR. Each `new: e` pair binds a new
                    -- attribute name (left) to a value expression (right) and
                    -- removes the operand attributes the value references. The
                    -- pairs reuse <arg-list> with field-init shorthand DISABLED
                    -- — the colon is required (P0017 on `replace { new }`).
                    -- Every value must COMPUTE (read ≥1 operand attribute via an
                    -- operator), restricted to Integer/Text (T0046); it desugars
                    -- through `extend` + `project` + `rename`. A bare attribute
                    -- reference only relabels → use `rename` (T0047). A constant
                    -- or a value reading no operand attribute removes nothing →
                    -- use `extend` (T0042). P0040 on a missing `{`.
<tclose-suffix> ::= 'tclose' [ '{' <ident> { ',' <ident> } '}' ] ; -- parse_tclose_suffix
                    -- Relational transitive closure. Postfix at pipeline
                    -- precedence (like <project-suffix> / <replace-suffix>);
                    -- wraps the operand in TCLOSE_EXPR. The brace-list is
                    -- OPTIONAL and UNORDERED: `R tclose { a, b }` is sugar
                    -- for `(R project { a, b }) tclose`, picking two columns
                    -- from a wider relation; bare `R tclose` requires the
                    -- operand to already be a binary relation. Direction-
                    -- agnostic: the result heading == the operand heading
                    -- (no from/to). Because the braces are optional this does
                    -- NOT reuse <ident-brace-list> (which makes them
                    -- mandatory); the bare form is not an error. P0041 on a
                    -- missing attribute name inside the braces, P0042 on a
                    -- missing closing `}`. `tclose` is a contextual keyword,
                    -- a valid identifier elsewhere.
<extend-suffix> ::= 'extend' <arg-list> ;                     -- parse_extend_suffix
                    -- Relational extend. Postfix at pipeline precedence (like
                    -- <replace-suffix>); wraps the operand in EXTEND_EXPR. Each
                    -- `new: e` pair adds a new attribute name (left) bound to a
                    -- computed value expression (right), KEEPING every operand
                    -- attribute (the dual of `replace`, which consumes the
                    -- attributes its value references). The pairs reuse
                    -- <arg-list> with field-init shorthand DISABLED — the colon
                    -- is required (P0017 on `extend { new }`), since a shorthand
                    -- would be the no-op identity `new: new`. P0043 on a missing
                    -- `{`. `extend` is a contextual keyword, a valid identifier
                    -- elsewhere.
<rename-suffix> ::= 'rename' <arg-list> ;                     -- parse_rename_suffix
                    -- Relational rename (relabel). Postfix at pipeline
                    -- precedence (like <replace-suffix>); wraps the operand in
                    -- RENAME_EXPR. Each `new: old` pair relabels the source
                    -- attribute `old` (right, a bare attribute reference) to
                    -- `new` (left); type- and cardinality-preserving. The strict
                    -- relabel-only partition of `replace`: a computed value is
                    -- rejected → use `replace` (T0030). The pairs reuse <arg-list>
                    -- with field-init shorthand DISABLED — the colon is required
                    -- (P0017 on `rename { new }`). `old` must exist (T0029) and
                    -- the result must stay a bijection (T0031). P0034 on a
                    -- missing `{`. `rename` is a contextual keyword, a valid
                    -- identifier elsewhere.
<wrap-suffix>   ::= 'wrap' '{' <wrap-pair> { ',' <wrap-pair> } '}' ; -- parse_wrap_suffix
<wrap-pair>     ::= <identifier> ':' <ident-brace-list> ;       -- (a WRAP_PAIR node)
                    -- Relational wrap (group attributes into tuple-valued
                    -- attributes). Postfix at pipeline precedence; wraps the
                    -- operand in WRAP_EXPR. Each pair binds a new tuple-valued
                    -- attribute name (left) to an UNORDERED brace-list of
                    -- existing attribute names (right) — NOT an expression. The
                    -- listed attributes are removed from the top level and
                    -- become the new attribute's tuple components. Each wrapped
                    -- attribute must exist (T0027), be wrapped at most once
                    -- (T0028); each new name must be fresh (T0031). P0044 on a
                    -- missing outer `{`, P0045 on a missing new name, P0046 on a
                    -- missing `:`, P0047/P0048/P0049 on the inner brace-list's
                    -- `{`/name/`}`, P0050 on a missing outer `}`. v1 declines the
                    -- SQL push (restructures in-process). `wrap` is a contextual
                    -- keyword.
<unwrap-suffix> ::= 'unwrap' <ident-brace-list> ;              -- parse_unwrap_suffix
                    -- Relational unwrap (expand tuple-valued attributes back to
                    -- their components, lifted to top level — the inverse of
                    -- `wrap`). Postfix at pipeline precedence; wraps the operand
                    -- in UNWRAP_EXPR. The unordered brace-list names the
                    -- tuple-valued attributes to expand: each must exist (T0027),
                    -- be listed once (T0028), and be tuple-valued (T0048); a
                    -- lifted component colliding with a survivor is T0031. P0051
                    -- on a missing `{`, P0052 on a missing name, P0053 on a
                    -- missing `}`. v1 declines the SQL push. Contextual keyword.
<group-suffix>  ::= 'group' '{' <group-pair> { ',' <group-pair> } '}' ; -- parse_group_suffix
<group-pair>    ::= <identifier> ':' <ident-brace-list> ;       -- (a GROUP_PAIR node)
                    -- Relational group (TTM GROUP — consume attributes into a
                    -- relation-valued attribute; the attributes named in NO
                    -- pair survive and partition the relation, one result
                    -- tuple per distinct survivor combination). Postfix at
                    -- pipeline precedence; wraps the operand in GROUP_EXPR.
                    -- Same production shape as <wrap-suffix>; the semantics
                    -- differ (cardinality-changing nest vs. heading rewrite).
                    -- Multi-pair group is SIMULTANEOUS — one partition by the
                    -- common survivors, each pair nesting its own components
                    -- (`{…}` is unordered, so Tutorial D's sequential
                    -- commalist is out; chain `group {…} group {…}` for that).
                    -- Each consumed attribute must exist (T0027) and be
                    -- consumed at most once across all pairs (T0028); each new
                    -- name must be fresh (T0031). P0032 on a missing outer
                    -- `{`, P0087 on a missing new name, P0088 on a missing
                    -- `:`, P0089/P0090/P0091 on the inner brace-list's
                    -- `{`/name/`}`, P0092 on a missing outer `}`. Never pushes
                    -- to SQL (a relation-valued cell has no flat-column form);
                    -- the operand fetch pushes, the nest runs in-process.
                    -- `group` is a contextual keyword.
<ungroup-suffix> ::= 'ungroup' <ident-brace-list> ;            -- parse_ungroup_suffix
                    -- Relational ungroup (TTM UNGROUP — unnest relation-valued
                    -- attributes back to top level: one result tuple per
                    -- combination of an outer tuple and one tuple from each
                    -- named RVA; an empty RVA contributes nothing). Postfix at
                    -- pipeline precedence; wraps the operand in UNGROUP_EXPR.
                    -- The unordered brace-list names the relation-valued
                    -- attributes to unnest: each must exist (T0027), be listed
                    -- once (T0028), and be relation-valued (T0100); a lifted
                    -- attribute colliding with a survivor is T0031 (rename
                    -- first). P0093 on a missing `{`, P0094 on a missing name,
                    -- P0095 on a missing `}`. Never pushes to SQL (like
                    -- `group`). Contextual keyword.
<transaction-expr> ::= 'transaction' <block> ;                 -- parse_transaction_expr
                    -- `transaction` is claimed only together with its
                    -- `[` (one token of lookahead); a bare `transaction`
                    -- is an ordinary NAME_REF.
<if-expr>       ::= 'if' <expr> 'then' <block>
                    [ 'else' <block> ] ;                       -- parse_if_expr (IF_EXPR)
                    -- `if`/`then`/`else` are contextual keywords. `then`
                    -- delimits the condition so it parses at full precedence:
                    -- `[` is otherwise ambiguous between a postfix index and
                    -- the ordered block, and a condition ending in an index
                    -- run (`if grid[r][c] then …`) can't be split from the
                    -- block positionally. Both arms are ordered <block>s
                    -- (bracket = ordered). `else` is optional — a bare
                    -- `if … then [ … ]` is the Unit-typed statement form.
                    -- P0059 if `then` is missing, P0060 on a missing then-block
                    -- `[`, P0061 on a missing else-block `[`. Typecheck:
                    -- condition Boolean (T0067); with `else` the arms unify
                    -- (T0068); without `else` the then-arm must be Unit
                    -- (T0069). Chain via nesting `else [ if … ]`.
<name-ref>      ::= <identifier> ;
<arg-list>      ::= '{' [ <named-arg> commalist ] '}' ;        -- parse_arg_list
<named-arg>     ::= <identifier> [ ':' <expr> ] ;              -- parse_named_arg
                    -- Field-init shorthand: a bare `<identifier>` (no colon)
                    -- means `<identifier>: <identifier>` — the value is the
                    -- same-named binding in scope, like Rust's struct
                    -- field-init shorthand. The parser wraps the name in a
                    -- NAME_REF (retroactive start_node_at) so the value view
                    -- is a name-ref and every consumer sees the explicit
                    -- form; no tokens are synthesized, so the CST stays
                    -- byte-lossless. Shorthand is enabled in call-position
                    -- <arg-list> and in <tuple-lit>, and DISABLED in
                    -- <replace-suffix> (the colon stays required there: a
                    -- shorthand `replace { x }` would be the no-op `x -> x`).
                    -- P0016 (no name); P0017 (no `:` where it is required).
<tuple-lit>     ::= '{' [ <named-arg> commalist ] '}' ;        -- parse_tuple_lit
                    -- Same grammar as <arg-list>; the wrapping node
                    -- kind (TUPLE_LIT vs ARG_LIST) distinguishes a
                    -- tuple value from a call-site argument list.
                    -- Field-init shorthand applies (e.g. `{a}` ≡ `{a: a}`).
                    -- Empty '{}' is the unit value, type Tuple {}.
<relation-lit>  ::= 'Relation' '{' <relation-lit-body> ;    -- parse_relation_lit
                    -- 'Relation' is a contextual keyword, claimed in
                    -- primary-expr position only together with its `{`;
                    -- a bare `Relation` is an ordinary NAME_REF. Its
                    -- sibling `reltrue` is the one-empty-tuple literal
                    -- `Relation { {} }`.
<relation-lit-body> ::= [ <expr> commalist ] '}' ;         -- parse_relation_lit_body
                    -- The '{' is already consumed; the body is a
                    -- comma-separated list of element **expressions**
                    -- (trailing comma allowed), each of which must be
                    -- tuple-typed — a tuple literal `{a:1}`, or a
                    -- tuple-valued name/call/… (`Relation { req }`).
                    -- Symmetric with <sequence-lit>; the tuple-typed
                    -- constraint is enforced in typecheck (T0096), not
                    -- here. Unterminated → P0033. Empty `Relation {}`
                    -- is `relfalse` — the nullary empty relation (empty
                    -- heading, zero tuples; the zero of the join
                    -- semiring). Shared with <tuple-set>.
<sequence-lit>  ::= 'Sequence' '[' [ <expr> commalist ] ']' ;     -- parse_sequence_lit
                    -- 'Sequence' is a contextual keyword, claimed in
                    -- primary-expr position only together with its `[`;
                    -- a bare `Sequence` is an ordinary NAME_REF. The
                    -- body is a comma-separated list of element
                    -- expressions, trailing comma allowed (P0056 if
                    -- unterminated). Empty `Sequence []`
                    -- parses cleanly. *Syntactically* a primary
                    -- expression, but the typechecker permits it only
                    -- as a `let` binding value (T0063 elsewhere); an
                    -- empty literal takes its element type from the
                    -- `let` annotation (`let s: Sequence Integer =
                    -- Sequence []`), else T0061.

<unknown-item>  ::= -- error recovery: any tokens until the next
                    -- top-level ';' at bracket-depth zero or EOF.
                    -- Emitted as PARSE_ERROR with diagnostic P0001.
                    ;                                          -- parse_unknown_item
```

`program`, `oper`, `let`, and `transaction` are **contextual
keywords** — the parser identifies them by lexeme at specific
syntactic positions; outside those positions they are regular
identifiers. Only the five Tier-1 words (and the word-operator
glyphs) are reserved outright, rejected at declaration sites with
P0096 — see the "Reserved words" section above.

### Deliberately not yet in the grammar

The following are decided design intent (see the rationale section
above) but not yet wired into the parser. Listed here so the omission
is explicit, not implied:

- **Statement forms** other than the ones in `<stmt>` above (`<let-stmt>`,
  `<var-stmt>`, the loop forms, `<load-stmt>`, `<truncate-stmt>`,
  `<delete-stmt>`, `<insert-stmt>`, `<update-stmt>`, `<return-stmt>`,
  `<assign-stmt>`, and `<expr> ';'`) — `mut`.
- **Type / relvar / constraint declarations** at the top level.
- **Literals**: sequence `[ … ]` in expression position. (Tuple
  `{ … }` literals and dot-prefix field access landed in Phase 18.
  Relation literals `Relation { … }` landed in Phase 19. Boolean
  literals `true` / `false` and infix `=`, `<>`, `<`, `>`, `<=`,
  `>=`, `and`, `or`, `where` landed in Phase 20.)
- **`mod`** (`Integer × Integer → Integer`, multiplicative precedence) and
  **unary minus** / negative literals. Still deferred. (Binary arithmetic
  `+`, `-`, `*`, `/` on `Integer` and concatenation `||` on `Text`/`Character`
  landed — they parse at prec 4/5 above comparison, see `<infix-op>`.)
- **Pattern matching**, **`if`/`else`**, **anonymous opers**.


## Parser diagnostics

Every diagnostic the parser emits has a stable `P####` code. Every
code in the syntax crate appears here; the hygiene-check script
enforces that.

| Code  | Trigger                                                 |
|-------|---------------------------------------------------------|
| P0001 | Expected a top-level declaration                        |
| P0002 | Expected program name                                   |
| P0003 | Expected `;` after program declaration                  |
| P0004 | Expected operator name                                  |
| P0005 | Expected `{` to start parameter heading                 |
| P0006 | Expected `[` to start operator body                     |
| P0007 | Expected `;` after operator declaration                 |
| P0008 | Expected `}` to close parameter heading                 |
| P0009 | Expected parameter name                                 |
| P0010 | Expected `:` after parameter name                       |
| P0011 | Expected type name                                      |
| P0012 | Unclosed operator body                                  |
| P0013 | Expected `;` after expression                           |
| P0014 | Expected expression                                     |
| P0015 | Expected `}` to close argument list                     |
| P0016 | Expected argument name                                  |
| P0017 | Expected `:` after argument name                        |
| P0018 | `let`/`var` binding is malformed (missing name, operator, or RHS) |
| P0019 | *Retired* (was: `transaction` not followed by `[`) — unreachable since the expression-head narrowing made a bare `transaction` a NAME_REF; reusable |
| P0020 | Expected database name (in `database <Name>;`)          |
| P0021 | Expected `;` after `database <Name>`                    |
| P0022 | Expected `{` to start key clause                        |
| P0023 | Expected key attribute name                             |
| P0024 | Expected `}` to close key clause                        |
| P0025 | Expected `relvar` after relvar kind                     |
| P0026 | Expected relvar name                                    |
| P0027 | Expected `{` to start relvar heading                    |
| P0028 | Expected `;` after relvar declaration                   |
| P0029 | Expected `}` to close tuple literal                     |
| P0030 | Expected field name after `.`                           |
| P0031 | *Retired* (was: expected `{` after `Relation`) — unreachable since the expression-head narrowing made a bare `Relation` a NAME_REF; reusable |
| P0032 | Expected `{` to start group list (code reused: the original P0032 — relation-literal element check — retired to typecheck T0096) |
| P0033 | Expected `}` to close relation literal                  |
| P0034 | Expected `{` to start rename list                       |
| P0035 | Expected `)` to close parenthesized expression          |
| P0036 | Expected `{` to start project list                      |
| P0037 | Expected project attribute name                         |
| P0038 | Expected `}` to close project list                      |
| P0039 | Expected `but` after `all` in project                   |
| P0040 | Expected `{` to start replace list                      |
| P0041 | Expected attribute name in tclose list                  |
| P0042 | Expected `}` to close tclose list                       |
| P0043 | Expected `{` to start extend list                       |
| P0044 | Expected `{` to start wrap list                         |
| P0045 | Expected new attribute name in wrap                     |
| P0046 | Expected `:` after wrap attribute name                  |
| P0047 | Expected `{` to start wrapped-attribute list            |
| P0048 | Expected attribute name in wrapped-attribute list       |
| P0049 | Expected `}` to close wrapped-attribute list            |
| P0050 | Expected `}` to close wrap list                         |
| P0051 | Expected `{` to start unwrap list                       |
| P0052 | Expected attribute name in unwrap list                  |
| P0053 | Expected `}` to close unwrap list                       |
| P0054 | Expected `{ … }` clause after the `update` target        |
| P0055 | *Retired* (was: expected `[` after `Sequence`) — unreachable since the expression-head narrowing made a bare `Sequence` a NAME_REF; reusable |
| P0056 | Expected `]` to close sequence literal                  |
| P0057 | Expected `]` to close index expression                  |
| P0058 | Expected index expression                               |
| P0059 | Expected `then` after the `if` condition                |
| P0060 | Expected `[` to start the `if` block                    |
| P0061 | Expected `[` after `else`                               |
| P0062 | Expected a loop variable name after `for`               |
| P0063 | Expected `:=` or `in` after the `for` loop variable     |
| P0064 | Expected `to` after the `for` lower bound               |
| P0065 | Expected `do` before the `for` loop body                |
| P0066 | Expected `[` to start the `for` loop body               |
| P0067 | `let` binding bound with `:=` (use `=`)                 |
| P0068 | `var` binding bound with `=` (use `:=`)                 |
| P0069 | Expected `do` before the `while` loop body              |
| P0070 | Expected `[` to start the `while` loop body             |
| P0071 | Expected `[` to start the `do` loop body                |
| P0072 | Expected `while` after the `do` loop body               |
| P0073 | Expected a target name after `load`                     |
| P0074 | Expected `from` after the `load` target                 |
| P0075 | Expected `[` to open the `load` order list              |
| P0076 | Expected `]` to close the `load` order list             |
| P0077 | Expected an attribute name in the order key             |
| P0078 | `builtin` operator must not have a body                 |
| P0079 | Expected `oper` or `relvar` after `builtin`              |
| P0080 | Expected type name after `type`                         |
| P0081 | Expected `=` in type declaration                        |
| P0082 | Expected `;` after type declaration                     |
| P0083 | Expected `module` after `use`                           |
| P0084 | Expected module name (leading, or after `::`)           |
| P0085 | Expected `;` after `use module <path>`                  |
| P0086 | Module-level `var` — module-level mutable state is a relvar; use `let` for a constant binding |
| P0087 | Expected new attribute name in group                     |
| P0088 | Expected `:` after group attribute name                  |
| P0089 | Expected `{` to start grouped-attribute list             |
| P0090 | Expected attribute name in grouped-attribute list        |
| P0091 | Expected `}` to close grouped-attribute list             |
| P0092 | Expected `}` to close group list                         |
| P0093 | Expected `{` to start ungroup list                       |
| P0094 | Expected attribute name in ungroup list                  |
| P0095 | Expected `}` to close ungroup list                       |
| P0096 | Reserved word used as an identifier — a declaration names one of the five reserved words or a word-operator glyph (soft: the diagnostic is emitted, the name still binds, parsing continues) |

Note: missing-type-after-`:` (let annotation), missing-type-after-`->`
(operator return clause), and missing-element-after-`Sequence` all
surface as `P0011` ("expected type name") via `parse_type_ref` — the
diagnostic message is identical and adding distinct codes would dedupe
to the same message.


## Lexer diagnostics

| Code  | Trigger                                              |
|-------|------------------------------------------------------|
| E0001 | Unexpected character (no token rule matched)         |
| E0002 | Unterminated `/* … */` block comment                 |
| E0003 | Unterminated string literal                          |
| E0004 | Empty character literal `''`                         |
| E0005 | Unterminated character literal                       |
| E0006 | Character literal contains more than one codepoint   |
| E0007 | Identifier may not start with `__`                   |
