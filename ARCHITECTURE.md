# Coddl — Architecture Sketch

A compiler for a relational language conforming to Date and Darwen's *Third Manifesto*. Query fragments compile to SQL and run against a pluggable storage backend (SQLite first, Postgres later); everything else compiles to LLVM IR and links against a small Rust runtime exposed through C ABI.

Coddl is its own D — not Tutorial D. It conforms to TTM's RM/OO Prescriptions and Proscriptions (§3) and designs its own surface syntax, IRs, and runtime around the principles below.

**Core principles** — binding on every design choice in this document. A proposal that violates one needs an explicit override, not a quiet exception.

1. **Performance.** Runtime cost is a first-class concern. The host language (Rust), the runtime (no GC, no managed RTS in user binaries), the FFI layer (zero-copy `#[repr(C)]` values), and the IR (Algebra A — push-down-friendly) are chosen for it. Features that force unavoidable overhead the user can't opt out of are rejected. When two designs are otherwise equivalent, the one with the lower steady-state cost wins.
2. **Long-term planning.** IR shapes, type representations, and crate boundaries are designed so deferred Manifesto features (VSS 7 heading polymorphism, transition constraints, type inheritance) and unanticipated extensions land without a rewrite. No painting into corners — keep the data structures wider than current need, and the boundaries semantic rather than expedient.
3. **Conformance over convenience.** When TTM prescribes a behavior, Coddl ships it — even when a non-conforming shortcut would be easier. Sanctioned design freedoms (host language, surface syntax, evaluation strategy, IR choice) are enumerated in §3 and bounded there.
4. **Few primitives, layered sugar.** Algebra A core operators (§4); operators-as-relations; no special cases. Surface sugar — `extend`, `where`, `summarize` — desugars during lowering. Sugar lives in one place, not woven through the IR.

## 1. Host language

**Rust** (chosen). Sum-type enums and exhaustive pattern matching are the right tools for AST/IR work; the language has no garbage collector and no managed runtime to drag into end-user binaries; FFI is `extern "C"` natively; and `#[repr(C)]` plus the borrow checker make the LLVM/runtime ABI tractable to keep correct over time. Performance and long-term-planning principles both push hard toward Rust over the GC'd alternatives we considered.

Stack:
- **Parser**: hand-rolled recursive descent over the lexer's token stream, driving a `rowan` green-tree builder. Same playbook as `rust-analyzer`: predictable error recovery, exact span control, and the parser maps 1:1 onto the productions in the grammar appendix. No combinator library.
- **LLVM**: emit LLVM IR as text and shell out to `llc`/`clang`. We deliberately avoid `llvm-sys`/`inkwell` — version-coupling churn, build complexity, and we don't need programmatic IR introspection in the foreseeable plan. Text emission is fast and forward-compatible.
- **Databases**: backend-specific crates behind a trait. `rusqlite` for SQLite, `postgres` (sync) for Postgres. Avoid `sqlx`'s compile-time SQL checking — we emit our own SQL and don't want a second SQL parser in our build.
- **Pretty-printing / formatting**: the `pretty` crate for SQL and LLVM IR emission where the structure benefits from combinators; plain `std::fmt::Write` where it doesn't.
- **Build**: a single Cargo workspace, one crate per subsystem (see §8). Release builds with LTO and `codegen-units = 1` for the compiler binary; the runtime is a `staticlib` so user binaries don't take a dynamic linker hit.
- **Runtime**: a Rust crate exposing `extern "C"` symbols (see §6). Compiled Coddl binaries link against it directly — no managed runtime, no allocator surprises. The RelIR→SQL emitter (`coddl-sqlemit`) is a single crate used by both the compiler and the runtime; no duplication, no FFI seam between them.

## 2. Pipeline

```
source.cdl
   │
   ▼  lex + parse (hand-rolled state machine + recursive descent; uniform named-argument prefix syntax — see §3)
  AST
   │
   ▼  name resolution, module/import resolution
  Resolved AST
   │
   ▼  type checking (possreps + headings; constraint inference)
  Typed AST
   │
   ▼  lowering — splits into two IRs at the relational boundary
   │
   ├──────────────► RelIR  (Algebra A core + sugar layer; see §4)
   │                  │
   │                  ▼  desugar to A core
   │                  ▼  optimize (push-down, FD-aware key inference, dead-attr elim)
   │                  │
   │                  ▼  SQL emit (dialect = backend method)
   │                  SQL string + parameter slots
   │
   └──────────────► ProcIR (SSA-ish, LLVM-shaped)
                      │
                      ▼  LLVM IR text emission (`pretty` crate)
                      │
                      ▼  llc / clang   (target triple selects native or wasm32)
                      object file → linked against libcoddl_runtime (Rust staticlib)
```

The two IRs meet only at query-call sites: ProcIR holds the relation as a `Relation` handle (see §9) plus the parameters it needs to bind; the runtime returns rows that ProcIR consumes as tuples.

Every frontend pass also returns a `Vec<Diagnostic>` alongside its (possibly partial) output — the CLI driver renders them to the terminal; `coddl-lsp` serializes them as `PublishDiagnostics` (see §12). The pipeline above is the happy path; on the unhappy path, partial results and diagnostics flow back together rather than the pipeline halting.

## 3. Conformance to the Third Manifesto

Coddl conforms to *The Third Manifesto* (Date & Darwen, 3rd ed., 2014). The RM/OO Prescriptions and Proscriptions listed below are binding on design choices throughout this document.

**Coddl is its own D.** Tutorial D is the Manifesto's reference D, useful as a study aid and prior-art benchmark, not a spec Coddl follows. Where TTM prescribes behavior, Coddl conforms. Where TTM is silent, Coddl picks the answer aligned with the core principles in the intro — convergence with Tutorial D's specific choice is incidental, not a goal. The design choices TTM doesn't dictate, and which this document fixes, are:

1. **Host language and runtime stack.** Rust + LLVM-text codegen + a C-ABI Rust runtime. See §1, §6.
2. **Surface syntax.** Uniform named-argument prefix style, in the spirit of the form the Manifesto's authors propose in ch. 5 (pp. 127–128) but never adopt. See "Surface syntax" below.
3. **Evaluation strategy.** Lazy relations, strict scalars. TTM doesn't address evaluation; this is our choice. See §9.
4. **Canonical RelIR.** Algebra A as the IR core, which the authors recommend for any industrial-strength D (Appendix A). See §4.

Anything beyond this list is *not* a sanctioned design freedom — propose explicitly and add to the list rather than slipping it in.

### Adopted (RM/OO Prescriptions and Proscriptions — non-negotiable)

- **Scalar types** carry possreps with selectors and THE_ accessors; named types are disjoint; no implicit coercion (RM Pre 1–5).
- **`Tuple H` and `Relation H`** are type generators with structural identity by heading (RM Pre 6–7). Tuple/relation type equality is set-equality of `{name: type}` pairs.
- **No nulls. Ever.** Missing information is handled by **vertical decomposition**: split the relvar so the absence of a fact is the absence of a tuple in a side relvar, rather than a placeholder in an attribute. This is the canonical TTM answer — see ch. 7 RM Pro 4 and exercise 7.9. The type system *permits* a user-defined sum-type scalar (e.g., an `Optional` with `Some`/`None` possreps), since arbitrary user-defined scalars are allowed, but it isn't the recommended approach and shouldn't be the first thing reached for. The SQL backend must never emit `NULL` for an attribute value, never emit `NULLABLE` columns, never use `IS NULL` predicates, and must wrap any operator that SQL would otherwise produce a null from (see §5).
- **No duplicate tuples**, **no ordinal-position semantics** for attributes or tuples, **no composite attributes** (use `Tuple`-typed attributes instead), **no domain-check override**, **no internal-level constructs in source** (RM Pro 1, 2, 3, 6, 8, 9).
- **No tuple-at-a-time operations on relvars or relations** (RM Pro 7). Iteration over a relation is only available via the `load` construct (§9) which orders, materializes into an array, then iterates the array — the iteration boundary forces a deliberate materialization.
- **First-class `Tuple` and `Relation` types**, including parameters, return values, attribute types (so relation-valued attributes are allowed) (RM Pre 6–7, 9–10, 13).
- **Compile-time type checking** (OO Pre 1). Type-constraint violations (a selector argument failing its `possrep`'s `constraint`) remain run-time.
- **Computational completeness** (OO Pre 3). Coddl is the whole language; no host required.
- **Explicit transactions, nested transactions** (OO Pre 4, 5).
- **Aggregate identity on empty sets** (OO Pre 6) — `sum`=0, `and`=`true`, `or`=`false`, etc.
- **Relvars are not domains; no pointer attributes** in database relvars (OO Pro 1, 2).
- **Observational equality** (RM Pre 8): two values are equal iff indistinguishable under every operator on their type.
- **Multiple assignment** with the Manifesto's stated semantics (RM Pre 21): expand sugar; fold duplicate targets via WITH; evaluate all RHSs; assign atomically; check database constraints at the end of the whole MA (not per individual assignment, not at COMMIT).
- **Database constraints checked at statement boundaries** (not deferred to COMMIT) (RM Pre 23).
- **The Assignment Principle for views** — an INSERT into a view must fail if the inserted tuple would not appear in the view's defining expression (RM Pre 21).
- **The catalog is itself a set of relvars** — metacircular, queryable by ordinary relational expressions (RM Pre 25).

### Adopted (RM Very Strong Suggestions worth committing to in v1)

- **System keys** (VSS 1): `DEFAULT` operator-invocation clauses, a relational `TAG` operator (window-function lowering: `ROW_NUMBER() OVER (PARTITION BY …)`), nonupdatable system-default attributes.
- **Candidate-key inference** (VSS 3), minimally: propagate FDs through project/equijoin/restrict and surface inferred keys to the catalog. Best-effort.
- **Transition constraints** (VSS 4): primed-relvar syntax (`S'`) in `CONSTRAINT` bodies; pre-image captured by the runtime over delta sets, not by SQL triggers.
- **Quota queries** (VSS 5): `RANK r BY (DESC attr AS rankcol)` desugaring at the parser, lowering to `RANK()`/`DENSE_RANK()` window functions.

### Not adopted (matching the current Manifesto edition)

- **Foreign-key shorthand** (former VSS 2) — the authors formally deleted this VSS in later editions, and Coddl follows suit. Users write the general subset-constraint form directly: `CONSTRAINT SP{S#} ⊆ S{S#}` (RM Pre 23). This is what FK shorthand desugared to anyway, and it sidesteps the positional-matching example the authors regretted.

### Deferred to a later milestone

- **Generalized transitive closure** (VSS 6) — depends on VSS 7. Ship plain `TCLOSE` first.
- **User-defined heading-polymorphic operators** (VSS 7). Design the type system so adding row/heading polymorphism later doesn't force a rewrite: keep headings first-class in the type representation, don't hardwire monomorphic dispatch.
- **Type inheritance** (OO Pre 2, IM Pres). Conditional in the Manifesto. Coddl omits inheritance in v1; if added, it conforms to Part IV of the Manifesto in full.

### Skipped

- **SQL migration** (VSS 8). Out of scope for v1. Influence on the design is limited to: keep the type system extensible enough to add a parallel `SQL_*` type family later, and keep built-in operator names addressable (don't hardwire `=` to one type).

### Surface syntax

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

- **Infix for binary operators**, symbolic *and* textual:
  - Symbolic: `=`, `<>`, `<`, `>`, `<=`, `>=`, `+`, `-`, `*`, `/`. The comparison operators `<=` and `>=` are polymorphic: scalar comparison on scalars (as ever); **subset** and **superset** on relations (`R <= S` iff every tuple in `R` appears in `S`; `S >= R` iff `R <= S`). `<` and `>` give strict subset / superset analogously. Identical headings are required for the relation overload — checked at compile time. There's no separate `subset` keyword; `<=` covers it.
  - Textual relational: `join`, `times`, `intersect`, `union`, `minus`, `where`.
  - Textual logical: `and`, `or`. (Both `Boolean × Boolean → Boolean`. `or` < `and` < `not` < comparison on the precedence ladder; final ordering deferred to the parser phase.)
  - Textual arithmetic: `mod`. (`Integer × Integer → Integer`. Binds at multiplicative precedence, alongside `*` and `/`. Defined only for `Integer` in v1 — extending to `Rational`/`Approximate` needs a separate semantic decision.)

  Reason: the named-prefix form is clumsy for ubiquitous dyadic ops on identifier-unfriendly names, and the textual binary ops all have natural infix readings from math and SQL. No-reserved-words still holds — `join` is recognized contextually in expression position; it remains a valid identifier elsewhere.

  **`times` and `intersect` are typed aliases of `join`.** Both lower to the same `AND` node in Algebra A (§4 — `AND` generalizes TIMES and INTERSECT). The aliases exist for intent-signaling and compile-time enforcement: `times` requires the two heading sets to be **disjoint** (otherwise the user meant `join`, not a cartesian product, and the type checker catches the slip); `intersect` requires the two heading sets to be **identical**. Both checks are static, both are zero-cost at runtime.

  **`union` and `minus` require identical headings.** `union` lowers to A-core `OR` (heading-agnostic relational union, restricted at the type level to matching headings since Coddl has no nulls). `minus` lowers to `AND NOT` (set difference is `R join (NOT S)` when headings match). Both checks are static; mismatched-heading attempts are rejected at compile time with a diagnostic.

  **`where` (restriction) is also infix and special-cased in two ways.** The right operand is a *predicate*, not another relation, and that has two consequences:

  - **Scope injection.** Identifiers in the predicate resolve against the left operand's heading first, the enclosing scope second. `SP where s# = supplier` reads as: `s#` is the SP attribute; `supplier` is a parameter from the enclosing `oper`. The parser and typechecker inject the left operand's heading into the predicate's name-resolution scope automatically. This is the first construct with a non-uniform scoping rule; every later construct that takes a predicate (`extend`, `summarize`'s aggregate expressions, possrep constraints) reuses the same machinery.
  - **Precedence.** `where` binds looser than `=`, `<`, `+`, `and`, `or` — the predicate is expected to be a full scalar expression. `R where x = 1 and y > 0` parses as `R where ((x = 1) and (y > 0))` without parentheses. Practically `where` sits at the bottom of the infix precedence ladder, alongside `union`/`minus` (each of those also "consumes everything to its right" in a chain like `R join S where p` → `(R join S) where p`). Full precedence table lands when the parser does — exact order is deferred until then.

- **Parenthesized positional for monadic operators**: `count(R)`, `sin(x)`, `is_*(...)`, `not(p)`. Single argument, name-free, conventional shape.

- **Named-prefix with braces for everything else** — n-ary or structured operands: selectors, `oper` calls in general, `extend`, `summarize`, `rename`, `group`, `ungroup`, `wrap`, `unwrap`. These all have meaningful name slots that would be lost in a positional form.

This eliminates the relational-algebra/scalar-op syntactic distinction the authors regret, and matches RM Pro 1 (no ordinal-position semantics) at the surface where it's easiest to enforce.

Operator precedence among the infix textual operators is a separate decision and is deferred until the parser starts on expressions — the examples here use parentheses to disambiguate (`(R join S) where { p }`).

### Brackets vs braces encode ordering

A consistent two-character distinction across the entire surface syntax:

- **`{ ... }` (curly braces) — unordered.** A set-like collection where position is meaningless. Used for named-argument lists, `Tuple` and `Relation` literals, heading declarations, and parameter lists in `oper` declarations. Reordering the contents preserves meaning: `write_line { message: "hi {x}", params: {x: "world"} }` and `write_line { params: {x: "world"}, message: "hi {x}" }` are identical programs.

- **`[ ... ]` (square brackets) — ordered.** A sequence where position is semantically significant. Used for `Sequence T` literals (`[1, 2, 3]`, `[tup1, tup2, tup3]`), operator bodies (statements run in order), `load` ordering specs, and any other context where the reader's expectation is "this is a sequence." Reordering changes meaning.

- **`( ... )` (parentheses)** — kept for expression grouping and for the small set of monadic operators that retain parenthesized positional form (`count`, `sin`, `is_*`).

This maps directly onto TTM. Tuples, relations, and headings have no ordinal position semantics (RM Pro 1); they get `{ ... }`. Procedural code is sequential by nature; it gets `[ ... ]`. The punctuation tells the reader which kind of collection they're looking at without having to recall any context.

### Identifier case

Coddl is case-sensitive: `foo` and `Foo` are distinct identifiers. The language uses three case styles, applied consistently to built-ins and recommended for user code:

- **lowercase / snake_case** — keywords (`program`, `oper`, `where`, `join`, `load`, `if`, `then`, `else`, ...), built-in operators (`and`, `or`, `not`, `count`, `sum`, `extend`, ...), built-in constants (`true`, `false`, `reltrue`, `relfalse`), and user-named operators, variables, attributes, and parameters.
- **PascalCase** — type names, both built-in (`Integer`, `Rational`, `Text`, `Character`, `Boolean`, `Tuple`, `Relation`, `Sequence`) and user-defined (`Customer`, `OrderLine`, `EmailAddress`); and relvar names by convention (`Customer`, `Suppliers`, `OrderLines`).

User code is not *required* to follow PascalCase for types and relvars — that's convention, not language. The language only enforces case sensitivity (so `customer` and `Customer` are different identifiers) and the canonical case of built-in identifiers (the `Integer` built-in is `Integer`, never `integer` or `INTEGER`).

Implications for the lexer and parser: identifiers are matched case-sensitively. Contextual keyword recognition (next section) looks for the lowercase form only — `Program` at the start of a file is a user identifier, not the keyword.

#### Identifier shape

- **Lexical class**: Unicode UAX #31 — `XID_Start` for the first character, `XID_Continue` for subsequent. The lexer NFKC-normalizes identifiers before comparison so visually equivalent character sequences denote the same identifier (e.g. `é` precomposed = `e` + combining acute).
- **Leading single underscore** (`_foo`) marks an identifier the developer is OK with being unused — the typechecker won't warn about unused locals or parameters whose name starts with `_`. Same convention as Rust.
- **Bare `_`** is the wildcard / "don't care" pattern. Reserved as a single-character form for (planned) pattern matching's catch-all branch and any other "I don't want to name this" slot.
- **Leading `__` (double underscore) is reserved for compiler-internal use** and rejected from user identifiers. This gives the desugarer, optimizer, and runtime a private namespace (`__plan_42`, `__tmp_join_lhs`, `__coddl_runtime_call`) that cannot ever shadow user code. snake_case with internal underscores (`foo_bar`, `write_line`, `_unused`) is unaffected — the rule is purely a leading-prefix check.

### Unicode operator glyphs

A small set of single-codepoint mathematical glyphs lex as **exact synonyms** for their ASCII / keyword counterparts. The lexer emits the same token either way; grammar productions name only the ASCII form, but `R ⋈ S` and `R join S` are interchangeable in source.

Glyphs assigned so far (more added as their corresponding operators are settled):

| ASCII | Glyph(s) | Codepoint(s) |
|---|---|---|
| `join` | `⋈` | U+22C8 |
| `union` | `∪` | U+222A |
| `intersect` | `∩` | U+2229 |
| `minus` | `∖` | U+2216 SET MINUS (**not** U+005C reverse solidus — that's the string-escape character) |
| `<=` | `≤`, `⊆` | U+2264, U+2286 |
| `>=` | `≥`, `⊇` | U+2265, U+2287 |
| `<` | `⊂` | U+2282 (mostly useful in the relational strict-subset reading; scalar `<` keeps its ASCII form) |
| `>` | `⊃` | U+2283 |
| `<>` | `≠` | U+2260 |

Deliberately **not** in the synonym set: Greek letters (`π σ ρ γ` — too easily mistaken for ordinary identifiers in non-math source), Boolean truth-value glyphs (`⊤ ⊥`), and the empty-set glyph (`∅`). The formatter normalizes to one canonical form per `format.edition`; the formatter rule is deferred until that edition lands but the default will likely be ASCII for searchability, with a project-level option to flip to glyphs.

### Comments

- **Line comments** start with `//` and run to the end of the line.
- **Block comments** are delimited by `/*` and `*/` and **nest**. `/* outer /* inner */ still outer */` is one well-formed comment; the lexer counts depth on each `/*` and `*/`. The motivation is purely ergonomic — commenting out a region that already contains a block comment Just Works, with no need to surgically convert the inner pairs first.

The lexer treats both kinds as trivia and attaches them to the CST per §13 so the formatter can preserve them. The choice of `//` over the `--` from Tutorial D / SQL is a deliberate move away from the SQL pedigree where it costs nothing — `--` collides with the binary minus and "negative literal" patterns under enough lookahead pressure that committing to it would constrain unrelated grammar choices.

### Literals

- **Text** literals: double-quoted, e.g. `"hello, world"`. Standard escape sequences `\n`, `\r`, `\t`, `\"`, `\\`, and `\u{HHHHHH}` for a Unicode codepoint (1–6 hex digits, value ≤ U+10FFFF, outside the UTF-16 surrogate range D800–DFFF). Multi-line is permitted — raw newlines are kept as-is.
- **Character** literals: single-quoted, exactly one codepoint, e.g. `'a'`, `'\n'`, `'\u{1F600}'`. The lexer rejects empty `''` and multi-codepoint `'ab'` at the lexical level. Escape syntax matches Text.
- **Boolean** literals: `true`, `false`.
- **Numeric** literals — three lexical shapes, one per numeric type:
    - `42`, `0xff`, `0b1010`, `0o17`, `0d99` → **`Integer`** (base prefixes are case-insensitive; `0d` is the explicit-decimal prefix).
    - `42.0`, `3.14` → **`Rational`** (digits-dot-digits; both sides need at least one digit; no `42.` or `.5`).
    - `42e0`, `4.2e1`, `1e-9` → **`Approximate`** (exponent required; mantissa integer- *or* rational-shaped).
  Underscores between digits are decoration and stripped before conversion: `1_000_000`, `0xff_ff_ff`. The exponent marker `e` and the hex digits `a`–`f` are case-insensitive; the formatter normalizes to lowercase. The three-shape split is unambiguous: the lexer picks one of `Integer`/`Rational`/`Approximate` from the literal's *form* alone, without inference.

### Method-style call syntax (UFCS via `self`)

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
- `x.possrep_name` (no braces) is the possrep accessor — returns the possrep view of `x`. Inherited from §7.
- `x.method_name { ... }` (with braces, including empty `{}`) is a method call.

The braces are the parser's disambiguation: `x.method` (no braces) is *always* an accessor; `x.method {}` is *always* a method call, even with zero arguments.

This mirrors UFCS in D / Nim and Rust's method syntax — but without an `impl` block: methods are just opers with a `self` parameter.

### Reserved words: none

Coddl has no hard-reserved identifiers. At the lexer level there is no `KEYWORD` token type — every alphanumeric/underscore/`#` token is an `IDENT`. The parser recognizes specific identifiers as keywords in specific syntactic positions (`program` at the start of a file, `oper` at a statement boundary, PascalCase identifiers in type position resolving against the type table, the built-in constants `true` / `false` / `reltrue` / `relfalse` in expression position, etc.).

This is a deliberate ergonomic choice for a relational language whose users will model real domains with attribute names like `name`, `type`, `from`, `to`, `order`, `value`, `with`, `by`, `and`. Hard-reserving any of those is a tax we don't want to pay. The cost is the prefix-only constraint on textual operators already noted above — `and`, `or`, `not` are recognized in expression position contextually, not as reserved tokens.

The TextMate grammar still pattern-highlights these words at the lexical level (highlighting is a UX hint, not a lex check); the LSP's semantic tokens correct mis-highlightings later where a user has used such a word as an identifier.

Revisit if the parser's context-sensitivity proves unmanageable in practice, but the bias is strongly toward keeping the user identifier space unfettered.

## 4. Two IRs, one boundary

### RelIR — Algebra A core with a sugar layer

The Manifesto's authors argue (Appendix A) that any industrial-strength D should be *mappable to* Algebra A — a foundational set of primitives in the spirit of predicate logic — even if surface syntax uses higher-level operators. Coddl takes that seriously: **RelIR's core is Algebra A**, and surface operators are sugar that desugars during the lowering pass.

**A core**: `AND` (natural join, generalizes TIMES and INTERSECT), `OR` (heading-agnostic union), `NOT` (relational complement), `REMOVE` (project-away one attribute — existential elimination), `RENAME`, plus `TCLOSE`. Minimally these reduce further to `REMOVE` + `NOR` (or `NAND`) + `TCLOSE`, but the seven above are the practical primitives.

**Sugar layer** (desugars to A core): `Project`, `Restrict` (surface `where`), `Join`, `Union`, `Minus`, `Intersect`, `SemiJoin`, `SemiMinus`, `Extend`, `Summarize`, `Group`, `Ungroup`, `Wrap`, `Unwrap` — PascalCase as Rust enum-variant names; the corresponding surface keywords are lowercase (`join`, `union`, `extend`, …). Crucially, **operators are themselves relations** in the A formulation: a scalar function `f(X, Y) -> Z` is an (n+1)-ary relcon `F{X, Y, Z}`, and surface `extend r add { c: X+Y }` desugars to `r JOIN (PLUS RENAME(X AS A, Y AS B, Z AS C))` at the A level. Surface `where`-clauses similarly desugar to JOINs against constant relations. This collapses much of the operator zoo into pure JOIN-and-REMOVE, which is what the optimizer actually wants.

Every RelIR node carries:
- a **heading** (RM Pre 9): `{attribute → declared type}`
- an **FD set** for candidate-key inference (VSS 3)
- a **constraint set** for constraint inference (RM Pre 23): the boolean predicates known to hold on the relation's tuples
- a **storage origin** flag: rooted in relvars (push to SQL) vs. rooted in materialized values (in-process executor) vs. mixed

The optimizer's job is to draw the SQL-vs-in-process cut as close to the leaves as possible (see §9).

### ProcIR — procedural SSA IR

SSA blocks with typed values, plus a small set of relation-aware ops:
- `query(plan_id, [params...]) -> Relation`
- `load(Relation, OrderSpec) -> Array<Tuple>` — the only sanctioned iteration path
- `assign_relvar(name, plan_id, [params...])` (relational assignment)
- `multi_assign([(target, plan_id, params)…])` — atomic, MA semantics per RM Pre 21
- `begin_tx / commit_tx / rollback_tx`

These lower to calls into the runtime ABI. There is no `force` op: relation expressions evaluate on each use against current relvar state; explicit materialization, when wanted, is `load ... array` or assignment to a temporary relvar.

**Backend-agnostic by design.** ProcIR is shaped for SSA codegen in general, not LLVM specifically — a long-term-planning concession that costs little now and preserves room to add backends without rewriting the IR. The IR carries no LLVM-specific intrinsic names, metadata, or calling conventions at the node level; per-backend specifics live in the codegen crate (§8).

- **LLVM IR text (v1).** Emit text, shell out to `llc`/`clang`. The same emitter covers native targets (x86-64, aarch64) *and* `wasm32-*` via the target triple — WASM-via-LLVM is essentially free at the codegen layer.
- **Cranelift (planned).** Both IRs are SSA with the same value-model surface; the lowering is largely a different printer over the same ProcIR walk. Use cases: REPL JIT for fast query iteration, and toolchain-free AOT for deployments that don't want `clang` in the image.
- **Direct WASM via `wasm-encoder` (optional).** Worth keeping the door open for browser/wasmtime targets that don't want LLVM at all in the build. Lower priority than Cranelift; revisit when the use case lands.

Runtime portability is the harder half — see §6 and §8 (Cargo features) for how the SQL backends get gated out of `wasm32-*` builds.

## 5. Storage abstraction

A pair of Rust traits, one for the (pure) SQL-emitting half and one for the (effectful) connection half:

```rust
trait Backend {
    type Conn: Conn;
    fn dialect(&self) -> Dialect;
    fn emit_select(&self, plan: &RelPlan) -> SqlString;
    fn emit_ddl(&self, schema: &Schema) -> Vec<SqlString>;
    fn type_map(&self) -> &TypeMap;                       // CoddlType ↔ SQL type
    fn open(&self, dsn: &Dsn) -> Result<Self::Conn>;
}

trait Conn {
    fn prepare(&mut self, sql: &SqlString) -> Result<StmtId>;
    fn bind_and_step<'a>(&'a mut self, id: StmtId, params: &[Value]) -> Result<RowIter<'a>>;
    fn materialize_temp(&mut self, heading: &Heading, rows: &[Tuple]) -> Result<TempRelRef>;
}
```

Crates: `coddl-backend-sqlite`, `coddl-backend-postgres`. Selection is a Cargo-feature on the runtime crate; the LLVM-emitted binary links against exactly one runtime that wraps the chosen `Conn`. If passing backends around as values gets clumsy with the associated-type trait, switch to a `dyn`-friendly `BackendOps` record-of-fn-pointers — the per-call dispatch cost is negligible against query latency. Decide once the second backend lands. Cargo features also gate SQL backends out of `wasm32-*` builds where the C dependencies of `rusqlite`/`postgres` don't link (see §6).

Keep SQL emission to a **portable subset** (CTEs, window functions, standard joins) and isolate dialect divergence behind backend methods. Golden-file tests per backend: `RelIR plan → expected SQL` for each dialect.

### Mandatory SQL emission rules (Manifesto-driven)

These are not optimizations; they're correctness requirements imposed by the Manifesto's proscriptions. The emitter enforces all of them by construction:

| Rule | Reason |
|---|---|
| `SELECT DISTINCT` on every projection; `UNION` never `UNION ALL` | RM Pro 3 (no duplicates). |
| Always enumerate columns explicitly in a deterministic (name-sorted) order. Never emit `SELECT *`. Never emit `INSERT … VALUES` without a column list. Never emit bare `UNION`/`INTERSECT`/`EXCEPT` — use `… CORRESPONDING …` (or simulate by aligning explicit lists). | RM Pro 1 (no ordinal attribute order). |
| Never declare a column `NULL`; always `NOT NULL`. Reject SQL DDL paths that would allow nullable columns. | RM Pro 4 (no nulls). |
| Outer joins are forbidden in lowered SQL. Coddl source has no construct that compiles to one; the type system can't express "this attribute might not have a value" as an attribute property. | RM Pro 4. |
| Aggregates: wrap to honor identity (OO Pre 6). Emit `COALESCE(SUM(x), 0)`, `COALESCE(MAX(x), CAST(<lowest> AS T))`, etc. AVG over empty is undefined — emit a guarded expression that signals an error if the result would be queried. | OO Pre 6. |
| Relational assignment `R := expr` compiles inside a transaction to `DELETE FROM R; INSERT INTO R (…) SELECT … FROM (…)` (or `TRUNCATE` + `INSERT` on Postgres). Single-tuple INSERT/UPDATE/DELETE in source desugars to a relational-assignment expression first; the backend never sees the singular form. | RM Pre 21, RM Pro 7. |
| Always emit explicit `BEGIN` / `COMMIT`. Never rely on SQL's implicit transaction start. Set constraints `IMMEDIATE` at session start; never `INITIALLY DEFERRED`. | OO Pre 4; RM Pre 23 (statement-boundary check). |
| Avoid SQL `CHARACTER` / `CHAR(n)` entirely; use `VARCHAR`/`TEXT`. SQL's `CHAR` pads with trailing blanks under equality — violates RM Pre 8. | RM Pre 8. |
| Every base table emitted from a relvar has a `PRIMARY KEY` from the relvar's declared candidate key (RM Pre 15). The candidate key with the fewest attributes wins ties; the rest become `UNIQUE`. The compiler verifies minimality before emission. | RM Pre 15. |
| `reltrue` / `relfalse` (nullary relations): emit as `(SELECT) WHERE TRUE` / `WHERE FALSE`. SQLite/Postgres tolerate this; non-conforming backends would need a synthesized dummy column. | RM Pro 5. |
| SQLite-specific: Coddl `Boolean` lowers to SQL `INTEGER CHECK (col IN (0, 1))`. Avoid the SQLite affinity-coercion footguns by always `CAST`-ing on `INSERT`. | dialect quirk. |

### Sending in-memory relations back into SQL

Same as before (§9, "Relations flowing back into SQL"): backend method `materializeIntoTemp` ships an in-memory relation to a temp table the next query can reference like a relvar. SQLite: temp tables / `carray`. Postgres: temp tables / `UNNEST` for small / `COPY` for large.

## 6. Runtime (`libcoddl_runtime`)

A Rust crate exposing `extern "C"` entry points, built as a `staticlib` by default (`cdylib` later if plugin loading lands). Compiled Coddl binaries link against it directly — no managed runtime, no garbage collector, no startup overhead beyond the program's own. Responsibilities:
- Own the DB connection pool.
- Cache prepared statements by `plan_id` (compiler assigns at codegen time).
- Marshal LLVM-side value structs ↔ backend parameter binders. `#[repr(C)]` Rust structs match the layout LLVM emits exactly; no marshaling cost beyond field reads, no FFI shim allocation. A single source-of-truth description (see §10 risk #8) generates both the LLVM struct text and the Rust `#[repr(C)]` declaration so they can't drift.
- Provide a row iterator the LLVM-emitted code can drive (cursor handle + `coddl_next` returning a tagged-union row).
- Host the in-process RelIR executor (§9) and the RelIR→SQL emitter (the same crate the compiler uses, `coddl-sqlemit` — no duplication, no FFI seam between compiler and runtime).
- Map errors to a single error code + thread-local message.

LLVM IR calls these exports as plain C functions. The runtime is where SQLite vs Postgres lives at runtime — the compiled program is backend-agnostic if we're disciplined about not leaking dialect-specific values through the ABI.

**Performance posture.** The runtime is on the hot path for every relation operation that crosses the SQL/in-process boundary. Allocate per-query with a bump arena; free at query completion (a typed arena per heading is the natural unit). Avoid `Box<dyn Trait>` on tuple values; specialize over heading or use a fixed-size value layout. Pull row buffers from prepared statements directly into Coddl tuple memory where the dialect permits — zero-copy is the default, copy only when alignment or lifetime forces it. Abort-on-panic (`panic = "abort"`) for release builds: smaller stack-unwinding tables and a single failure mode at the FFI boundary.

**FFI boundary discipline.** Values crossing into LLVM-emitted code are `#[repr(C)]` or primitive. No Rust enums-with-payload across the boundary unless tagged-C-style. No `Vec`/`String` raw pointers without an explicit owner declaration. The discipline is enforced by a single layout-description module in the runtime crate, mirrored from there into LLVM codegen.

**Portability and backends as features.** SQL backends are Cargo features on the runtime crate (`sqlite`, `postgres`). `wasm32-*` builds drop these — the C dependencies of `rusqlite`/`postgres` don't link to wasm32-unknown-unknown — and either run with only the in-process executor (materialized relations, no DB) or proxy SQL through wasm host imports if a wasmtime/JS host is in play. Same crate split, different feature set at build time.

**Why Rust over plain C for the runtime.** A C `libcoddl_runtime` would be ~50–300 KB smaller as a `staticlib`; nothing else recommends it for our case. The two non-trivial runtime jobs — the in-process RelIR executor and the RelIR→SQL emitter — are tree walks over sum types, which Rust enums + pattern matching handle naturally and C reinvents painfully. The SQL emitter is the same crate the compiler uses; a C runtime would either duplicate it (two versions to keep in lockstep forever — against long-term planning) or call into a Rust crate (a Rust runtime with extra steps). Connection pooling and prepared-statement caching are markedly less code against `rusqlite`/`postgres` than against `sqlite3.h`/`libpq-fe.h`. Where binary size or non-Rust embedding ever does matter, the hot value-marshaling layer can drop to `#![no_std]` Rust or a small C TU without touching the executor or emitter — picking Rust now doesn't lock out a leaner future.

## 7. Type system

### Scalar types

A scalar type is a named, finite set of values disjoint from every other scalar type. Each user-defined scalar type carries one or more **possible representations** (possreps) — abstract representations made up of named, typed components — and a (possibly trivial) `CONSTRAINT` predicate that defines which possrep tuples denote real values of the type (RM Pre 4–5, p. 144–151).

For every possrep `PR` of type `T` the system synthesizes:
- A **selector** of declared type `T`, one parameter per component (selector name = possrep name). Every value of `T` must be producible by an all-literal selector invocation.
- A **THE_C accessor** per component `C`: read-only in source position; pseudovariable in target position (`THE_C(V) := x` is sugar for `V := PR(…, x in slot C, …)`).

**Type constraints** (the `possrep`'s `constraint` predicate) are checked at every selector invocation — that's the sole choke point because values of `T` can only be constructed via the selector. Type-constraint violations are run-time errors; argument-type mismatches are compile-time.

**Built-in scalar types (v1)**: `Integer`, `Rational`, `Approximate`, `Text`, `Character`, `Binary`, `Byte`, `Boolean`. PascalCase per §3 "Identifier case." Everything else — `Date`, `Timestamp`, `Uuid`, fixed-width numerics, decimal, currency — is a user-defined scalar type with one or more declared possreps. This is the modeling exercise TTM Appendix C walks through; Coddl ships a small standard library of these definitions but they aren't built into the language. Each built-in has fixed mappings to (a) LLVM type, (b) SQLite affinity + `CHECK` constraints where needed, (c) Postgres type; user-defined scalars get their mappings via possrep components.

**Three numeric types** — `Integer`, `Rational`, `Approximate` — with no implicit conversion between them. `Integer` is mathematically unbounded (a bignum at runtime); `Rational` is exact rational arithmetic (also potentially unbounded); `Approximate` is bounded-precision floating-point (maps to f64). The literal shapes (§3 "Literals") pick one without inference: `42` is `Integer`, `42.0` is `Rational`, `42e0` is `Approximate`. Code that needs Approximate-cost arithmetic on integer values writes `42e0`, not `42`. Users who need a bounded fast integer (e.g. `Int32`, `Int64`) define it as a user-defined possrep-constrained scalar over `Integer` (see §10 risk #9).

**`Text` and `Character` are separate types.** `Text` is an opaque character string — you cannot index into it (`t[2]` is a type error), cannot ask its length in code points without explicit conversion, and cannot pattern-match on its internal representation. `Character` is a single Unicode code point. A planned standard-library function (TBD spelling) converts `Text` to `Sequence Character` and back; that's the only sanctioned route between the two. The split matches Rust's `String` / `char` distinction and TTM's Appendix-A "scalar is atomic" rule (`Text`'s opacity is what lets the backend store it as `TEXT` / `VARCHAR` / a hash, depending on workload, without leaking implementation through indexing). Coddl deliberately departs from TTM's `CHARACTER` (a.k.a. `CHAR`) shorthand-for-string convention — see TTM ch. 6 p. 134 — because Coddl needs the names for two distinct types.

**`Binary` and `Byte` mirror the same opacity split.** `Binary` is an opaque byte blob (`b[2]` is a type error; backend may store as `BLOB` / `bytea` / a hash / a chunk-table without leaking through indexing). `Byte` is a single octet (0–255). The planned conversion functions are `oper to_bytes { self: Binary } : Sequence Byte` and `oper from_bytes { bytes: Sequence Byte } : Binary`, paralleling `to_codepoints` / `from_codepoints` on `Text`. Decisions deferred: bitwise operators on `Byte` (`&`, `|`, `^`, `~`, `<<`, `>>` — settle when a concrete need surfaces), explicit `Byte` ↔ `Integer` conversion, UTF-8 encoders / decoders between `Text` and `Binary` (`to_utf8` is fallible-free; `from_utf8` needs the planned sum-type story for invalid input).

`Integer` is mathematically unbounded per TTM, which forces big-integer arithmetic at runtime — a real cost against the performance principle. Whether to also ship bounded-width built-ins (`Int32`/`Int64`) as primitives, or to keep them as user-defined possrep-constrained scalars over `Integer`, is an open decision (§10 risk #8).

**No implicit coercion.** Distinct named scalar types are disjoint; `Integer` and `Rational` cannot be silently mixed. Equality `=` is type-monomorphic per RM Pre 8 ("indistinguishable for all operators on T").

**Static operator overloading is permitted.** A few comparison operators resolve to distinct underlying operators depending on the operand type family — most notably, `<=` and `>=` are scalar comparison on scalars and **subset** / **superset** on relations (`<` and `>` give strict subset / superset). The same identifier names two operators; the type checker picks which based on operand types at compile time. RM Pre 8 monomorphism is preserved because each underlying operator is type-monomorphic; the surface `<=` is just a shared spelling, the same way `+` can be spelled by `Integer` addition and `Rational` addition without violating RM Pre 8.

**No nulls.** Period. The type system has no nullable-attribute facility. Missing information is a database-design problem the user solves through **vertical decomposition** — splitting the relvar so the absence of a fact is the absence of a tuple in a side relvar (the canonical TTM answer; ch. 7, RM Pro 4). A user-defined sum-type scalar (`Optional` with `Some`/`None` possreps) is permitted by the type system but not the recommended approach. The SQL backend never sees a request to emit a NULL.

### Type generators

- `Tuple { a: T, b: U, … }` and `Relation { a: T, b: U, … }` are type generators producing structurally-identified types: `Tuple H1 = Tuple H2` iff `H1 = H2` as sets of `{name: type}` pairs. Same for `Relation`. Attribute order is immaterial. Both generators may take zero attributes (`reltrue` and `relfalse` are the only inhabitants of `Relation { }` — see naming note below).
- **`Tuple {}` is the unit type** — the type of a tuple with no attributes. It has exactly one value, written `{}` (the empty tuple literal). This is Coddl's analogue of Rust's `()`, Swift's `Void`, or the unit type in ML. An `oper` declared without an explicit return clause implicitly returns `Tuple {}`, and a body whose tail expression is `{}` (or whose last statement leaves nothing on the tail) yields the unit value. The two spellings `Tuple {}` and the value `{}` are unambiguous in context — one appears in type position, the other in expression position.
- `Sequence T` is the ordered counterpart — a finite ordered list of values of element type `T`, duplicates allowed, position significant. It's the procedural-side companion to `Relation`: where `Relation H` is an unordered set of tuples (RM Pro 1, 3), `Sequence Tuple H` is an ordered list of tuples (the canonical iteration form, see §9 `load`). The element type `T` may be any type — primitives (`Sequence Integer`), tuples (`Sequence Tuple H` — the typical case), or even relations (`Sequence Relation H` — useful for results of a parametric query over many parameter sets, or for relation-valued batches). The brackets-vs-braces rule applies: `Sequence` literals use `[v1, v2, v3]`. Two `Sequence T1` and `Sequence T2` are equal iff `T1 = T2` and the elements are pairwise equal in order.
- Headings may include relation-valued and tuple-valued attributes (nesting permitted; RM Pre 6–7).
- A *relvar* is a named variable of some `Relation H` type. Per RM Pre 14, every relvar has at least one declared candidate key (RM Pre 15), possibly the empty key (which forces cardinality ≤ 1). Coddl classifies relvars by lifetime and provenance, with one of the following kinds at declaration time — a database relvar (`real`/`base` — backed by storage; or `virtual` — a view) or an application relvar (`private` to the running program; or `public` — the program's view onto a slice of the database). The same four-kind classification appears in Tutorial D (ch. 5 p. 105) because the underlying distinctions are real ones, not because we're copying it.

#### Naming note: `reltrue` and `relfalse`

The two inhabitants of the type `Relation {}` (the nullary relation type — relation with empty heading) are called `reltrue` (cardinality 1, containing the empty tuple) and `relfalse` (cardinality 0, the empty relation). TTM and Tutorial D call them `TABLE_DEE` and `TABLE_DUM`, opaque even to readers who know TTM. Coddl renames them after their semantic role: `reltrue` is the multiplicative identity of the join semiring and behaves like boolean true under projection-away-of-everything; `relfalse` is the zero of the same semiring.

In terms of the type generators, the literal forms decode as:

- `relfalse` ≡ `Relation {}` (an empty relation literal — no tuples).
- `reltrue`  ≡ `Relation { Tuple {} }` ≡ `Relation { {} }` (a relation literal containing the one and only empty tuple).

The `Relation { … }` syntax is contextual: in **type** position it's the type generator with a heading; in **value** position it's a relation literal whose body is a comma-list of tuple-valued expressions. The empty form `Relation {}` is the value form (the type form needs no inhabitants to be named). The empty tuple `{}` may also be written `Tuple {}` in expression position — the `Tuple` constructor and the bare braced literal are equivalent for tuple values.

### Relations are fully first-class

Relations can be bound to variables, passed to and returned from operators, stored in tuples, nested inside other relations, used as function arguments and results everywhere a scalar can. The calling convention treats them uniformly (see §9).

### Type inference and constraint inference

Type inference for relational expressions is mandatory and mechanical from operator semantics (RM Pre 18): every RelIR node's heading is the heading of its operands transformed by its operator. The optimizer further runs:
- **FD propagation** for candidate-key inference (VSS 3) — best-effort.
- **Constraint propagation** (RM Pre 23): predicates known to hold on operands propagate through restrict, project, join, extend, etc. Used for view-constraint checking and as optimizer hints.

### Where constraints can live

Integrity constraints attach only to **database relvars** (real, virtual). Coddl does not support constraints on application relvars (private or public), tuple variables, or scalar variables — there's "no logical reason why it should not," as TTM acknowledges (ch. 5 p. 106), but the cost in implementation complexity outweighs the payoff for the use cases we've identified so far. Revisit if a concrete need surfaces.

## 8. Project layout (Cargo workspace)

```
coddl/
  Cargo.toml                       # workspace
  crates/
    coddl-diagnostics/             # shared span + diagnostic types (used by every frontend crate)
    coddl-syntax/                  # lexer + recursive-descent parser, CST (rowan) + AST view
    coddl-types/                   # type checker, type representation
    coddl-relir/                   # relational IR + optimizer
    coddl-procir/                  # procedural IR (backend-agnostic SSA)
    coddl-sqlemit/                 # RelIR → SQL (dialect-agnostic core; used by compiler and runtime)
    coddl-execlocal/               # in-process RelIR executor over materialized relations
    coddl-backend-sqlite/          # Cargo feature on the runtime
    coddl-backend-postgres/        # Cargo feature on the runtime
    coddl-codegen-llvm/            # ProcIR → LLVM IR text emission (v1)
    coddl-codegen-cranelift/       # ProcIR → Cranelift (planned; REPL JIT + toolchain-free AOT)
    coddl-codegen-wasm/            # ProcIR → wasm-encoder (optional; revisit when needed)
    coddl-runtime/                 # extern "C" staticlib linked into compiled binaries
    coddl-driver/                  # CLI: compile, run, repl, fmt
    coddl-lsp/                     # tower-lsp language server; thin adapter over the frontend crates (see §12)
    coddl-fmt/                     # canonical formatter — same library behind `coddl fmt` and the LSP (see §13)
  editors/
    vscode/                        # VSCode extension: TextMate grammar + language client (see §12)
  tests/
    golden/                        # SQL emission goldens per backend
    e2e/                           # compile + run end-to-end
  examples/
```

Release builds: LTO on, `codegen-units = 1` for the driver and runtime crates; `panic = "abort"` for the runtime (smaller unwinding tables, single failure mode at the FFI seam). `wasm32-*` targets build the runtime with `--no-default-features` to drop the SQL backend crates.

## 9. Execution model

**Relations are lazy.** Scalars are strict. A relation expression is a thunk: it doesn't run at construction, only when something needs its tuples — iteration via `load`, being shipped into another query, being assigned to a relvar, being compared with `=`, being passed to a user-defined operator that consumes it. There is **no `force` keyword** in Coddl; each use re-evaluates the expression against current relvar state. (Laziness itself is design choice #3 in §3; TTM doesn't address evaluation strategy.) Equality is by value (heading + tuple set), so two relations built by different routes that yield the same tuples are equal regardless of evaluation history (RM Pre 8).

Because relations are first-class, the calling convention has to be uniform: any function that takes a relation must accept a value it can read, re-query, and pass onward. The runtime may memoize a handle's result when it can prove the source relvars haven't changed since the previous use, but that's an optimization invisible to the user.

### Iteration: the `load` primitive

There is no tuple-at-a-time access to relvars or relations (RM Pro 7). The only iteration primitive is `load`, which forces the relation, imposes an order, and writes the tuples into a local array:

```
var A: Sequence Tuple { S#: S#, QTY: QTY };
load A from ( SP where P# = P#('P1') ) { S#, QTY } order ( asc S# );
do i := 1 to count(A) [
    -- process A[i]
];
```

`A` here has type `Sequence Tuple { S#: S#, QTY: QTY }` — an ordered list of tuples. `load` populates the sequence from a relation with a given order; the counted `do` loop walks it by position. The old `array` keyword from the Tutorial D-style precursor is gone — `Sequence T` is the proper type, not a sigil.

`load` is the syntactic and semantic gate between the set-oriented and procedural worlds: it forces the relation, imposes an order (the order is part of the operation, not a property of the relation), and writes the tuples into a local array. The array is then iterable by a counted `do` loop. This is the *only* sanctioned path; the compiler rejects any other attempt to step through tuples one at a time.

The reverse direction — `load <relvar target> from <array var ref>` — is also supported: it assigns the (set-valued) projection of the array's tuples back into a relvar. Useful for round-tripping procedurally-built arrays into relational form.

### Multiple assignment

`A1, A2, …, An ;` is a single statement with the semantics of RM Pre 21:
1. Expand all syntactic shorthands (INSERT/UPDATE/DELETE/THE_ pseudovariable) into `target := expr` form.
2. Fold duplicate targets by rewriting `Vq := Xq` as `Vq := WITH Xp AS Vq : Xq` and dropping the earlier assignment. Repeat.
3. Evaluate every RHS expression. Capture results.
4. Apply all assignments to their targets atomically.
5. Check every applicable database constraint at the end of the whole MA (not between assignments).

The procedural IR therefore has a `multi_assign` primitive, not just a sequence of individual `assign` calls. The runtime evaluates all RHSs first (against the pre-MA database state), then commits the writes in one logical step, then runs constraint checks.

### Transactions

`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` are explicit (OO Pre 4). Nested transactions are supported (OO Pre 5): a nested `BEGIN` starts a child; child `COMMIT` is conditional on the parent; child `ROLLBACK` undoes only the child's work. The SQL backend uses SAVEPOINT for child transactions, but the runtime tracks the parent/child relationship explicitly because SQL `SAVEPOINT` doesn't model true nesting.

A relation handle captured before a write within the same transaction **re-evaluates on use** and so sees post-write state — the consequence of the lazy/thunk semantics above. If the user wants to freeze the pre-write tuples, they `load` the relation into an array (or assign it to a private relvar) before the write. This avoids any pre-image / copy-on-write machinery in the runtime.

### Relation values at runtime

A first-class relation is one of three things, behind a single `Relation` handle:

1. **Plan-backed** — a `plan_id` plus its already-bound parameters. The default. Each use re-evaluates against current relvar state. The runtime may memoize the result when source-relvar invalidation is provably absent, but that's an optimization, not a semantic guarantee.
2. **Materialized** — a runtime-owned buffer of tuples (arena-allocated, or a backend temp table for large ones). Used when tuples are already in memory: relation literals (`Relation { tup1, tup2 }`), results of the in-process executor, in-memory inputs being shipped back into SQL via temp table.
3. **Cursor** — a live result set being drained. Compiler-only optimization for `load ... order (...)` flows where the array is consumed once and never escapes — lets the runtime stream rows from the backend into the array slot-by-slot instead of buffering them all.

Materialization strategy:
- Small (under a threshold, say 10k tuples or N bytes): in-memory arena, columnar or row, indexed on demand if hit by a join.
- Large: backend temp table (`CREATE TEMP TABLE`), so the next query that uses it can still push down.

### Relations flowing back into SQL

This is the part first-class relations make non-trivial: a relation value built or filtered in procedural code may be the input to a subsequent query. Options the runtime needs to handle, picked per backend:

- **SQLite**: register `carray`/virtual-table modules, or `CREATE TEMP TABLE` + bulk insert. Start with temp tables.
- **Postgres**: `UNNEST` over arrays for small relations; `COPY` into a temp table for larger ones; table-valued parameters via temp tables are the portable bet.

The backend trait gets one more method: `materialize_into_temp(conn, heading, rows) -> TempRelRef`, and SQL emission can reference a `TempRelRef` as if it were a relvar.

### Plan registration and execution

- Each compile-time query becomes a `plan_id` with a SQL template and parameter signature.
- Codegen registers all plans at process start: `coddl_register_plan(id, sql, param_types, result_heading)`.
- At call sites, ProcIR computes parameters (including any `TempRelRef`s built from in-memory relations), calls `coddl_exec(plan_id, params) -> Relation`, and either iterates, materializes, or hands the handle onward.

### Plans built at runtime

Because relations are first-class, you can write functions whose query shape depends on which relation is passed in. The compiler can't always pre-bake the SQL for these. Two strategies, layered:

1. **Specialize when possible.** Monomorphize over relation headings at compile time (like Rust generics): every concrete call site gets its own `plan_id`. Covers most of the practical cases.
2. **Plan-at-runtime fallback.** For genuinely dynamic relational composition, the runtime owns a small RelIR→SQL emitter (the same crate the compiler uses) and emits + prepares SQL on first call, caching by plan shape. This is why `coddl-sqlemit` must be usable as a library, not just a compiler phase.

### In-process relational executor

You also need to evaluate algebra over relations the SQL backend never saw — relations constructed in code, the results of joining a materialized relation with another materialized relation when neither came from the DB, etc. A small **in-process executor** for RelIR over materialized relations (volcano-style iterators, hash joins, sort-merge) is a required component, not optional. It's also gold for tests and the REPL.

So the runtime has two execution engines side-by-side:
- the SQL backend, for any subplan rooted in relvars,
- the in-process executor, for subplans rooted in materialized values.

The RelIR optimizer's job is to draw the line between them as low (close to the leaves) as possible: push everything that touches a relvar into SQL, do the rest in-process.

## 10. Risks worth deciding early

1. **Materialization thresholds.** First-class relations mean the runtime constantly chooses between in-memory and temp-table representation. Pick a default policy (size-based, with an explicit `@materialize` / `@stream` annotation as escape hatches) before you write the runtime allocator.
2. **How honest about SQL are you willing to be?** Operators-as-relations (§4) makes surface `extend`/`where`/`summarize` all reduce to JOIN at the A level, which is push-down-friendly — but pushing down requires SQL-expressible scalar functions. Start by pushing pure-relational algebra; evaluate scalar UDFs in the in-process executor unless they have a known SQL equivalent.
3. **Possrep canonicalization.** RM Pre 8's "indistinguishable" rule means a user-defined type with a non-canonical possrep (e.g., `Rational { N: N, D: D }` without a coprime constraint; polar `Point { R: R, θ: θ }` for the origin allowing any θ) breaks equality. The compiler must require possrep constraints that force a canonical form, or refuse to synthesize `=` and warn loudly. Decide whether canonicalization is the user's responsibility (require, refuse otherwise) or the system's (rewrite to canonical form behind the scenes) before shipping user-defined types.
4. **Transition constraint pre-image capture.** VSS 4's primed-relvar syntax requires the runtime to keep a snapshot of every relvar touched within a statement until the constraint check completes. For multi-relvar transitions this is non-trivial; decide whether the snapshot is row-level (delta sets) or relvar-level (copy-on-write) before adding VSS 4 to the runtime.
5. **The Assignment Principle for views.** RM Pre 21: inserting into a view must fail if the inserted tuple wouldn't appear in the view. Generically computing this from a virtual-relvar definition is hard; the Manifesto allows the system to refuse views it can't update. Decide early: which view shapes Coddl will accept updates against, which it will reject at definition time, which it will accept and check at runtime.
6. **Heading polymorphism design space.** VSS 7 is deferred for v1, but the type system must keep headings first-class so that future row-polymorphic operator signatures don't require a rewrite. Don't bake monomorphic dispatch into the IR; allow heading-typed parameters at the type-rep level even if no surface syntax yet exposes them.
7. **Specialize vs. runtime-plan.** Specializing relation-polymorphic functions on heading at compile time keeps things simple but can blow up code size in pathological cases. Have the runtime planner (§9, "Plans built at runtime") ready from the start so you can fall back when specialization isn't viable.
8. **FFI struct-layout single source of truth.** ProcIR's tuple/value layout, the Rust runtime's `#[repr(C)]` types, and the LLVM IR text the compiler emits all describe the same memory. Drift between them is silent at compile time and a debug nightmare at runtime. Build a single layout description (a Rust type with derives that generates both the LLVM struct emission and the matching `#[repr(C)]` declaration) before the second value type lands. Same for the tagged-union row representation. This is a long-term-planning bill we pay now or pay tenfold later.
9. **`Integer` precision and arithmetic cost.** TTM's `INTEGER` (Coddl's `Integer`) is mathematically unbounded; shipping it as the only integer built-in forces bignum arithmetic on what 99% of users will use as a machine int. Decide before user-defined possrep machinery ships: keep `Integer` unbounded and lean on user-defined `Int32`/`Int64`, or add bounded built-ins at the cost of one more documented type. The performance principle leans toward bounded built-ins; the conformance principle leans toward keeping the TTM-derived name unbounded.

## 11. First milestone

1. Lex + parse the uniform-prefix-syntax core (RM Pre 1, 6–10, 13–14, 18): scalar declarations, possrep/selector, relvar declarations, `join`, `where`/`restrict`, `extend`, simple `summarize`, `rename`, `project`. Multiple assignment. **Establish the spans-on-every-node and diagnostics-as-values discipline from §12 here** — these are project-wide invariants, not LSP-conditional. The parser does error recovery from day one (no bailing on the first syntax error; emit `PARSE_ERROR` CST nodes and continue).
2. Type-check headings, possreps, and selector signatures. Enforce no-nulls, no-duplicates at the type level. Verify candidate keys are declared and minimal. Type errors propagate via `Error` types, not cascades.
3. Lower to RelIR (sugar → A core during the same pass). Emit SQLite SQL honoring every rule in §5.
4. Hand-write the Rust runtime that runs the SQL and prints rows — no LLVM yet. Implement explicit transactions and multiple assignment.
5. Add the in-process RelIR executor for `Relation` literals and constructed relations.
6. Add ProcIR + the LLVM codegen crate with `load`, counted `do` loops, and `query → relation → load → iterate`. Link the runtime as a `staticlib` and confirm the FFI struct layout matches the LLVM emission.
7. Add the Postgres backend behind the same `Backend` trait. Confirm the golden SQL tests fork cleanly per dialect.
8. Add user-defined scalar types with possreps, selectors, `the_` ops, and possrep constraints. Confirm equality works through the possrep round-trip.

VSS adoptions (system keys/TAG, FK shorthand, candidate-key inference, transition constraints, RANK quota queries) come after the milestone above is end-to-end on a toy program.

## 12. Editor tooling (LSP + VSCode extension)

A VSCode extension shipping a TextMate grammar for instant lexical highlighting, paired with `coddl-lsp` — a Rust language server built on the same frontend crates as the compiler. v1 scope is the two capabilities currently committed: **syntax highlighting and diagnostics (warnings/errors)**. Hover, go-to-definition, find-references, completion, and semantic-token enhancements are designed-for but not v1 work.

### Crates and project shape

- `coddl-diagnostics` — shared diagnostic data type: `(file_id, byte_range)` span + severity + code + message + optional related-spans. Every frontend crate (`coddl-syntax`, `coddl-types`, `coddl-relir`, `coddl-sqlemit`) produces and consumes this type. The CLI driver renders to terminal; `coddl-lsp` serializes to `PublishDiagnostics`.
- `coddl-lsp` — language server binary on `tower-lsp` over stdio. Owns document state and request dispatch; no analysis logic of its own — it calls into the frontend crates and forwards their output. Adding hover / go-to-def later is straightforward once `coddl-types` exposes symbol tables.
- `editors/vscode/` — VSCode extension (TypeScript). Ships the TextMate grammar (`syntaxes/coddl.tmLanguage.json`), language configuration (brackets, comments, indent rules), and a client that spawns `coddl-lsp` from `PATH` or a configured location.

Tree-sitter (more accurate, incremental highlighting) is a possible upgrade later; maintaining a second parser in lockstep with `coddl-syntax` is real cost and defers until concrete demand surfaces.

### Discipline this imposes on the frontend (lands in milestone 1, not "when the LSP arrives")

The LSP isn't an add-on bolted on at the end — its requirements shape the rest of the frontend. These constraints land on the compiler from day one, in line with long-term planning:

1. **Spans on every AST/IR node.** Every token, every AST node, every typed-AST node, every diagnostic carries `(file_id, byte_range)`. Retrofitting spans is a project-wide refactor — write them in from the first lexer token.
2. **Error recovery in the parser.** The recursive-descent parser produces a best-effort CST with `PARSE_ERROR` nodes wrapping unrecoverable token ranges rather than failing on the first syntax error. The type checker treats `Error` types as propagating-but-not-cascading — don't pile a hundred type errors on top of one parse error.
3. **Diagnostics-as-values.** No `panic!` or `eprintln!` for user-visible errors anywhere in the frontend. Every pass returns its diagnostics in a `Vec<Diagnostic>` alongside the (possibly partial) result. CLI and LSP differ only in presentation.
4. **Pure analyses.** Every frontend pass is `fn(Input) -> (Output, Vec<Diagnostic>)` — no globals, no I/O, no hidden state. The LSP can call any pass on any buffer at any time.

### Performance posture

v1: full re-parse + re-typecheck per buffer edit. Coddl programs are small and the Rust frontend is fast; latency won't be the bottleneck on realistic files. **Long-term planning:** route analyses through `salsa` (rust-analyzer's incremental-computation library) once response latency matters. The pure-analysis discipline above makes that migration mechanical rather than architectural — every pass is already shaped like a salsa query.

### Out of scope for v1

Code lenses, refactorings, debug adapter protocol. Sockets for these live in `coddl-lsp` once core diagnostics + hover + go-to-def land. Formatting (`coddl fmt` and `textDocument/formatting`) is in scope — it's covered separately in §13 because it has its own design implications for the parser.

## 13. Code formatter (`coddl fmt`)

A canonical formatter for Coddl source, exposed two ways from one library: `coddl fmt` (driver subcommand, à la `cargo fmt`) and `textDocument/formatting` in `coddl-lsp` (format-on-save). Both paths call into `coddl-fmt`; there is no second implementation.

### CST over AST-with-trivia

This is the load-bearing decision. The compiler's typechecker doesn't care about whitespace or comments — they're noise for analysis. The formatter cares about every byte. Three options:

1. **AST + side-channel trivia.** Parser emits the AST and a parallel list of (byte-range, trivia) entries. Formatter walks the AST and consults the list. Cheap up-front; every formatter pass re-decides "where does this comment attach?", and edge cases proliferate.
2. **AST with attached trivia.** Each AST node holds leading/trailing trivia. Bloats the AST for every consumer; non-formatter passes pay the memory cost.
3. **Concrete syntax tree (CST) + AST view.** Parser produces a lossless tree — every token, every trivia, every byte. The AST is a typed view derived from the CST; the typechecker walks the AST, the formatter walks the CST, both share the same backing storage. This is `rust-analyzer`'s approach via `rowan`.

**Coddl picks option 3.** Long-term planning: the formatter, the LSP semantic-tokens path, and incremental re-analysis under `salsa` (§12) all want a lossless tree. Retrofitting one is a parser rewrite — the kind of corner-painting this project explicitly avoids. `coddl-syntax` produces a CST from day one; `coddl-types`, `coddl-relir`, and friends consume an AST view derived from it.

`coddl-diagnostics::Span` carries through unchanged — it's still `(file_id, byte_range)`, which the CST can produce for any node trivially.

### Formatting rules (v1)

The formatter is opinionated and has few knobs. A `fmt` whose output drifts between versions or wobbles with bikeshedding is worse than a stricter one. Initial rules:

- **Indent**: 4 spaces (`indent_width` config; revisit if real demand surfaces).
- **Line width**: 100 columns soft; hard if a single token can't be split.
- **Braces**: `{` on the same line as the keyword/operator that opens them; `}` on its own line aligned with the opener — except trivial single-line bodies (`OP { x: 1, y: 2 }`) which stay inline up to the line-width limit.
- **Named arguments inside braces**: one space after the colon (`name: value`), one space after the inter-arg comma. One per line if any single arg makes the whole call exceed the line width; otherwise stay on the line. No alignment of names or colons across lines (it churns under add/remove).
- **Operator spacing**: one space around `=`, `<`, `>`, `+`, `-`, `*`, `/`, `,`; no space around `.`.
- **Trailing commas**: required in multi-line bracketed lists, forbidden in single-line ones (so adding then removing a wrap is idempotent).
- **Blank lines**: preserve user blank lines between top-level items, collapsed to at most one consecutive blank.
- **Comments**: preserved as-is, attached to the following node by default. Block-leading `//` comments stay on their own line; trailing `//` comments stay trailing. `/* */` block comments (including nested ones) keep their existing line breaks; the formatter doesn't reflow content inside them.

Idempotency is a unit-test invariant: `fmt(fmt(x)) == fmt(x)` for every input in `examples/` and `tests/`.

### Edition versioning

Formatter output is versioned, à la `rustfmt`'s editions. A project's `coddl.toml` carries `format.edition = "2026"` (or whichever); default = newest edition the compiler knows. Edition bumps are explicit opt-in; old projects keep their formatting until they update. This buys the freedom to evolve the rules without breaking every committed file in every downstream project.

### Performance posture

Format-on-save needs to be fast: milliseconds, not tens of milliseconds. The CST walk is O(n); the printer is O(n); no re-parsing inside the formatter. The frontend already serves both the CLI and the LSP from the same pure passes (§12), so the formatter inherits the same discipline — `fn(source) -> (formatted, Vec<Diagnostic>)`, no globals, no I/O.

### Out of scope for v1

Auto-import sorting, comment reflow at line-width limits, configurable rules beyond `indent_width` and `format.edition`, format-only-the-diff (`coddl fmt --check` is in scope; rustfmt-style range-only formatting in the LSP can land later). Add these once the rules above stabilize and the idempotency tests stick.

## 14. Memory model

> **Status.** This section is a working set of defaults — push back on it if a proposal conflicts. Most of TTM and the items in §3 are settled (no nulls is settled — RM Pro 4); the memory model below is a design *direction* we hold to until something better comes along. Flag conflicts; we'll resolve explicitly.

Coddl avoids both tracing garbage collection and Rust-style borrow tracking. It does so by being a value-semantics language with no user-facing references — neither piece of machinery is needed because the situations they exist to handle are unrepresentable. The implementation strategy is **atomic reference counting + copy-on-write + persistent data structures + per-scope arenas** — Swift's ARC + Clojure's persistent collections + Erlang's per-process heaps, three production-proven techniques that compose without conflicting.

### Why no tracing GC

Tracing GC exists primarily to reclaim cycles in the reference graph. Coddl's data graph is cycle-free by construction:

- Tuples, relations, and scalars are values (RM Pre 8 observational equality).
- OO Pro 2 forbids pointer attributes — relations can't reference each other by identity.
- Immutable values can only reference things that existed at the time of their construction, so the reference graph is a DAG.
- Closures capture by value (see "Discipline" below), so no closure can introduce a back-edge.

A DAG of refcounted values frees correctly in topological order when a root refcount hits zero. There are no cycles to collect; refcounting is sufficient.

### Why no borrow checker

Borrow checking prevents two co-existing references where one mutates — use-after-free, iterator invalidation, data races. Coddl makes those situations unrepresentable:

- Values are passed by value; the runtime decides whether that's an `Rc` bump or an actual copy.
- No `&` / `&mut` / `Box` / `Rc` in the surface language.
- Mutable locals (`let mut x = …; mut x = …;`) are stack slots that don't escape; no shared references to them exist.
- "Mutation" of a heap value (`mut xs = xs ++ [item];`) produces a new value. If the original had a single owner, copy-on-write turns it into in-place mutation; otherwise structural sharing makes the new value cheap.

The borrow checker's job — preventing aliased mutation — is done by the *type system* (no way to obtain a mutable alias), not by lifetime tracking.

### Surface vs implementation: two layers

Value semantics is a property of the *surface language*, not the compiled output. The user never sees a pointer, never sees an allocation, never sees a lifetime; the compiler and runtime emit pointers, stack frames, heap allocations, and refcount operations everywhere they help performance. Coddl is aiming for a **production-grade implementation** — the playbook is identical to what Swift, OCaml, and ML-family compilers already do at scale.

| Layer | What it sees |
|---|---|
| Source / AST / typed-AST | Values. No `&`, no allocation, no lifetime. |
| ProcIR (SSA) | SSA values with concrete representations — `Tuple { a: Integer }` is a register-resident scalar; `Text` is `*RcBox<TextRepr>`. |
| LLVM IR / machine code | Explicit `alloca`, `getelementptr`, `load`, `store`, refcount intrinsics, native ints, native pointers. |

What the compiler does with the surface guarantee, behind the scenes (none of this is user-visible, none of it requires user annotation):

- **Escape analysis** stack-allocates values that don't outlive their function — no heap touch, no refcount ops.
- **Move optimization** transfers ownership when the caller's copy is dead (refcount `1 → 1`, not `1 → 2 → 1`). A small Coddl-aware pass plus LLVM's optimizer take care of this.
- **Refcount elision** removes `incref`/`decref` pairs that cancel within one function.
- **Scalar replacement of aggregates (SROA)** breaks up tuples never observed as a whole into register-resident scalars.
- **Specialisation** monomorphizes relation-polymorphic operators per heading at compile time; the runtime sees concrete types and concrete layouts (§9 "Plans built at runtime" covers the fallback).
- **Small-value inlining** keeps small `Integer`s, `Character`s, `Boolean`s, `Byte`s unboxed, and likely small `Text`/`Binary` too — small-string-optimization-style.

Stack vs heap vs arena at runtime is decided by the compiler from data-flow analysis, not by user annotation:

| On the stack | On the heap (refcounted) | In a per-scope arena |
|---|---|---|
| Primitives | `Text`, `Binary` beyond an inline-storage threshold | Per-query / per-transaction scratch |
| Non-escaping tuples (post escape analysis) | `Sequence T` buffers | Materialised intermediate relations |
| `let mut` locals | `Relation H` plan handles + materialized rows | Lex / parse output for one source file |
| Short-lived refcount cells | Closure captures that outlive their frame | The CST for one buffer |

The two layers exist deliberately: the user reasons about *values*; the compiler reasons about *representations*. That separation is what lets Coddl have a clean value-semantics language *and* native-speed compiled output — neither paying GC tax nor demanding lifetime annotations from the user.

### Implementation strategy

| Layer | Mechanism |
|---|---|
| Primitives (`Integer`, `Rational`, `Approximate`, `Boolean`, `Character`, `Byte`) | Unboxed value types on the stack. `Integer` is a bignum, so it's boxed under the hood with small-integer optimization. |
| Boxed values (`Text`, `Binary`, `Tuple H`, `Relation H`, `Sequence T`) | Heap-allocated, **atomic reference counting**, freed at refcount = 0. |
| Compound updates (sequence concat, tuple-field update, relation insert) | Structural sharing + copy-on-write. If refcount = 1, mutate the buffer in place; otherwise allocate a new one referencing the old's tail. |
| Per-query / per-transaction scratch | Bump arena, freed wholesale at scope end. |
| Mutable locals | Stack slots holding a value (boxed or unboxed). Rebinding decrements old refcount, increments new. |
| Cross-thread sharing | Atomic refcount; Coddl values are `Send + Sync` for free because they're immutable. |

**What we lose vs. Rust**: zero-cost moves. We always pay one atomic refcount op on heap-value assignment. **What we gain vs. tracing GC**: predictable, low-latency reclamation; no stop-the-world pauses; the runtime stays tiny.

### Discipline (defaults — push back if a proposal conflicts)

These are the working assumptions that keep the model honest. They are *not* commandments — flag a conflict and we'll resolve it explicitly.

1. **No `&mut` / `&` / `Box` / `Rc` in the surface language.** What looks like "a reference to a tuple" in other languages is just "a tuple value" in Coddl. A method-receiver `self` is passed by value.
2. **No back-pointers in tuples or relations.** Already enforced by OO Pro 2.
3. **Closures capture by value.** Anonymous opers capture refcounted values; closing over a mutable local copies the *current* value, not the binding.
4. **No reference / box / pointer type at all.** Including indirectly via recursive type definitions that would let "a tuple containing a relation containing this tuple" exist. The typechecker rejects recursive type definitions; if we ever want them, we add cycle detection at the value-construction site rather than weakening the model.
5. **Mutating methods are surface sugar.** `customer.rename { new_name: "Bob" }` desugars to `mut customer = customer.rename { new_name: "Bob" };` in the caller's scope. The pure function returns a new value; the rebind happens at the call site. COW makes this cheap in the common case.

### When the model bends

Probably revisit if any of the following come true:

- Performance benchmarks show atomic-refcount overhead dominates in a realistic workload.
- A real use case needs shared mutable state (e.g. concurrent transaction coordination — though that's the runtime's responsibility, not the surface language's).
- Recursive type definitions turn out to be valuable enough to want them with value-level cycle detection.

In each case the path is: proposal → flag the conflict with this section → resolve explicitly (change the model or find a way to express the use case within it). We don't silently grow GC machinery or lifetime annotations into the language.

### Languages we cherry-pick from

| From | Idea taken | Not taken |
|---|---|---|
| Rust | Sum types, pattern matching, expression-based blocks, formatter-as-tool | Borrow checker, lifetime parameters |
| Haskell | Pure functions by default, parameterised types, laziness as a per-type design choice (relations only) | Monadic IO, total laziness, type-level programming |
| Swift | ARC + COW + value types, method-style on free functions, sum types via enums with payloads | Class inheritance, protocol-oriented runtime polymorphism |
| Erlang | Per-scope arenas, all-values-passable, immutability default | Dynamic typing, the actor model in user space |
| Go | Simplicity, formatter-enforced style, no implicit conversions | `interface{}` escape hatch, `nil`, share-by-channel as the only concurrency tool |
| Clojure | Persistent data structures, REPL workflow | Dynamic typing, JVM coupling |
| OCaml | Pattern matching, sum types, eager evaluation | Module-system complexity, functor-heavy organisation |

The result reads like none of them because the *combination* — TTM relational core + Rust-style ADTs + Swift-style ARC + Erlang-style scope arenas + Haskell-style purity + Go-style simplicity — is genuinely its own thing.
