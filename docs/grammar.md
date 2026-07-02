# Grammar — surface syntax

The authoritative spec for Coddl's surface syntax — the precise form of the language the parser currently accepts, plus the design rationale behind every choice that doesn't immediately follow from TTM (see [conformance.md](conformance.md)).

This doc has two parts: **rationale** (the design decisions — why prefix-named-args, why no reserved words, why brackets-vs-braces, etc.) and the **productions** (the EBNF the parser implements, lexical and syntactic).

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
- **Textual relational**: `join`, `times`, `intersect`, `compose`, `union`, `minus`, `where`.
- **Textual logical**: `and`, `or`. (Both `Boolean × Boolean → Boolean`. `or` < `and` < `not` < comparison on the precedence ladder; final ordering deferred to the parser phase.)
- **Textual arithmetic**: `mod`. (`Integer × Integer → Integer`. Binds at multiplicative precedence, alongside `*` and `/`. Defined only for `Integer` in v1 — extending to `Rational`/`Approximate` needs a separate semantic decision.)

Reason: the named-prefix form is clumsy for ubiquitous dyadic ops on identifier-unfriendly names, and the textual binary ops all have natural infix readings from math and SQL. No-reserved-words still holds — `join` is recognized contextually in expression position; it remains a valid identifier elsewhere.

**`times` and `intersect` are typed aliases of `join`; `compose` is a sibling with a different lowering.** `join`, `times`, and `intersect` all lower to the same `AND` node in Algebra A (see [relir.md](relir.md) — `AND` generalizes TIMES and INTERSECT); `compose` lowers to `AND` followed by `REMOVE` of the shared attributes. All four exist for intent-signaling and compile-time enforcement — every check below is static and zero-cost at runtime:

- **`join`** requires the two headings to **partially overlap** — share at least one attribute, but **not** be identical. With no overlap the result would be a Cartesian product the user almost never means by accident, so the typechecker rejects it and suggests `times`. With *identical* headings the result would be a set intersection, so it rejects it and suggests `intersect`. The three AND-family operators therefore partition the heading relationship completely and exclusively: **disjoint → `times`, partial overlap → `join`, identical → `intersect`** — each operand pair has exactly one legal spelling.
- **`times`** requires the two headings to be **disjoint**. With any attribute in common the typechecker rejects it and suggests `join`.
- **`intersect`** requires the two headings to be **identical**. Anything else is rejected (it names the differing attributes).
- **`compose`** requires **partial overlap** — the same legal domain as `join`: it joins on the common attributes and removes them. It is meaningful only when both derived sets are non-empty: the shared attributes `A ∩ B` (the join/remove key) and the symmetric difference `A △ B` (the result heading). With no overlap (`A ∩ B` empty) the typechecker rejects it and suggests `times` (a disjoint compose is just a Cartesian product with nothing to remove). With identical headings (`A △ B` empty) every attribute would be removed, so the result is always the nullary relation regardless of the data — the typechecker rejects it and suggests `intersect`. A proper subset/superset is fine (it's partial overlap): `{a,b,c} compose {b,c}` joins on `{b,c}`, removes them, and keeps `{a}`.

**`union` and `minus` require identical headings.** `union` lowers to A-core `OR` (heading-agnostic relational union, restricted at the type level to matching headings since Coddl has no nulls). `minus` lowers to `AND NOT` (set difference is `R join (NOT S)` when headings match). Both checks are static; mismatched-heading attempts are rejected at compile time with a diagnostic.

**`where` (restriction) is also infix and special-cased in two ways.** The right operand is a *predicate*, not another relation, and that has two consequences:

- **Scope injection.** Identifiers in the predicate resolve against the left operand's heading first, the enclosing scope second. `SP where s# = supplier` reads as: `s#` is the `SP` attribute; `supplier` is a parameter from the enclosing `oper`. The parser and typechecker inject the left operand's heading into the predicate's name-resolution scope automatically. This is the first construct with a non-uniform scoping rule; every later construct that takes a predicate (`extend`, `summarize`'s aggregate expressions, possrep constraints) reuses the same machinery.
- **Precedence.** `where` binds looser than `=`, `<`, `+`, `and`, `or` — the predicate is expected to be a full scalar expression. `R where x = 1 and y > 0` parses as `R where ((x = 1) and (y > 0))` without parentheses. Practically `where` sits at the bottom of the infix precedence ladder, alongside `union`/`minus`. Full precedence table lands when the parser does — exact order is deferred until then.

**`project` (projection) is a *postfix* operator, not infix.** `R project { a, b }` narrows the relation's heading to the named attributes — the right operand is a brace-list of bare attribute names (structurally identical to a `key { … }` clause), not another expression, so it doesn't fit the infix `<lhs> op <rhs>` shape. It is parsed as a postfix suffix at *pipeline precedence* — the same altitude as `where`, and gated to the top level so it binds to the whole pipeline rather than to a higher-precedence operand such as a `where` predicate. `R where p project { a }` reads as `(R where p) project { a }`; the reverse order `R project { a } where p` also parses, nesting left. It is the first postfix relational operator; later ones (`replace`, `extend`, `rename`, `tclose`, `wrap`, `unwrap`, and future `summarize`) reuse the same pipeline slot. `project` remains a contextual keyword — a valid identifier everywhere else (no reserved words).

### Named-prefix with braces — the only call form

Every operator invocation is `name { … }` (or its dot-method sugar `R.method { … }`, see "Method-style call syntax"): selectors, `oper` calls, `extend`, `summarize`, `replace`, `group`, `ungroup`, `wrap`, `unwrap`, and so on. **There is no positional call form** — Coddl has no `f(x)` syntax; parentheses are for expression grouping only (see "Brackets vs braces encode ordering"). Arguments are named (`name: expr`); a brace may instead hold a bare list of attribute *names* where that is the operand shape (`project { a, b }`, `key { a, b }`). The binary relational operators — `join`, `times`, `intersect`, `compose`, `union`, `minus`, `where` — are **infix only**; there is no named-prefix brace variant for them.

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

## Reserved words: none

Coddl has no hard-reserved identifiers. At the lexer level there is no `KEYWORD` token type — every alphanumeric/underscore/`#` token is an `IDENT`. The parser recognizes specific identifiers as keywords in specific syntactic positions (`program` at the start of a file, `oper` at a statement boundary, PascalCase identifiers in type position resolving against the type table, the built-in constants `true` / `false` / `reltrue` / `relfalse` in expression position, etc.).

This is a deliberate ergonomic choice for a relational language whose users will model real domains with attribute names like `name`, `type`, `from`, `to`, `order`, `value`, `with`, `by`, `and`. Hard-reserving any of those is a tax we don't want to pay. The cost is the prefix-only constraint on textual operators noted above — `and`, `or`, `not` are recognized in expression position contextually, not as reserved tokens.

The TextMate grammar still pattern-highlights these words at the lexical level (highlighting is a UX hint, not a lex check); the [LSP](lsp.md)'s semantic tokens correct mis-highlightings later where a user has used such a word as an identifier.

## Unicode operator glyphs

A small set of single-codepoint mathematical glyphs lex as **exact synonyms** for their ASCII / keyword counterparts. The lexer emits the same token either way; grammar productions name only the ASCII form, but `R ⋈ S` and `R join S` are interchangeable in source.

| ASCII | Glyph(s) | Codepoint(s) |
|---|---|---|
| `join` | `⋈` | U+22C8 |
| `union` | `∪` | U+222A |
| `intersect` | `∩` | U+2229 |
| `minus` | `∖` | U+2216 SET MINUS (**not** U+005C reverse solidus — that's the string-escape character) |
| `<=` | `≤`, `⊆` | U+2264, U+2286 |
| `>=` | `≥`, `⊇` | U+2265, U+2287 |
| `<` | `⊂` | U+2282 (relational strict-subset reading; scalar `<` keeps its ASCII form) |
| `>` | `⊃` | U+2283 |
| `<>` | `≠` | U+2260 |

Deliberately **not** in the synonym set: Greek letters (`π σ ρ γ` — too easily mistaken for ordinary identifiers in non-math source), Boolean truth-value glyphs (`⊤ ⊥`), and the empty-set glyph (`∅`). The [formatter](fmt.md) normalizes to one canonical form per `format.edition`.

## Literals

- **Text** literals: double-quoted, e.g. `"hello, world"`. Standard escape sequences `\n`, `\r`, `\t`, `\"`, `\\`, and `\u{HHHHHH}` for a Unicode codepoint (1–6 hex digits, value ≤ U+10FFFF, outside the UTF-16 surrogate range D800–DFFF). Multi-line is permitted — raw newlines are kept as-is.
- **Format-string** literals: a Text literal prefixed by `f` with the `f` **fused to the opening quote** (no space), e.g. `f"Hello, {name}!"`. Same body and escapes as a Text literal, plus `{name}` placeholders (a single attribute name) and `{{` / `}}` for literal braces. Its type is `FormatText` (see [typecheck.md](typecheck.md)), which exists only as the `template` argument of `format` — there is no `Text → FormatText` conversion, so a runtime `Text` can never be used as a template. Only the exact adjacency `f"` triggers it: a bare `f`, `f { … }`, `f "x"` (with a space), and `xf"x"` all stay an ordinary identifier (optionally) followed by a plain string. Lexical form → type, like the numeric shapes below. The lexer does **not** validate placeholders; that is a typecheck-time concern (T0055–T0059).
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

The current grammar accepts a top-level sequence of `program` and
`oper` declarations. Inside an operator body, the statement layer
recognizes expression statements only; the expression layer supports
identifier references, single-token literals, and brace-delimited call
expressions. Every rule below carries a comment naming the parser
function that implements it.

```
<root>          ::= { <item> } ;                              -- parse_root
<item>          ::= <program-decl>
                  | <database-binding>
                  | <public-relvar-decl>
                  | <private-relvar-decl>
                  | <oper-decl>
                  | <unknown-item> ;                          -- parse_item

<program-decl>  ::= 'program' <identifier> ';' ;              -- parse_program_decl

<database-binding> ::= 'database' <identifier> ';' ;          -- parse_database_binding
                       -- Binds this program to a catalog. The compiler
                       -- discovers <name>.cddb and <name>.cdstore from
                       -- the declared name. Absent → program uses no
                       -- public relvars.

<public-relvar-decl>  ::= 'public' <relvar-with-heading> ;        -- parse_public_relvar_decl
                          -- Application-side relvar exposed to the catalog.

<private-relvar-decl> ::= 'private' <relvar-with-heading> ;       -- parse_private_relvar_decl
                          -- Application-side relvar internal to the program.

<relvar-with-heading> ::= 'relvar' <identifier>
                          <heading>
                          { <key-clause> }
                          ';' ;                                -- parse_relvar_with_heading
                          -- Shared tail of the `public` / `private`
                          -- relvar productions. The kind keyword has
                          -- been consumed at the dispatch site.

-- `base relvar` and `virtual relvar` in `.cd` source parse via the
-- corresponding `parse_base_relvar_decl` / `parse_virtual_relvar_decl`
-- shared with the `.cddb` parser; the typechecker emits T0014 because
-- those kinds belong in `.cddb`. See docs/cddb-grammar.md for those
-- productions.

<oper-decl>     ::= 'oper' <identifier> <heading>
                    [ <return-clause> ]
                    <block> ';' ;                              -- parse_oper_decl
<return-clause> ::= '->' <type-ref> ;                          -- parse_return_clause

<heading>       ::= '{' [ <param> commalist ] '}' ;            -- parse_heading
<param>         ::= <identifier> ':' <type-ref> ;              -- parse_param
<type-ref>      ::= 'Sequence' <type-ref>                       -- parse_type_ref
                  | <identifier> ;
                    -- `Sequence T` is the one generator-applied type
                    -- form: a nested element type-ref (e.g.
                    -- `Sequence Integer`, `Sequence Sequence Text`).
                    -- `Tuple H` / `Relation H` are not yet a <type-ref>
                    -- (see "Deliberately not yet in the grammar").

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
                  | <for-stmt>
                  | <truncate-stmt>
                  | <delete-stmt>
                  | <insert-stmt>
                  | <update-stmt>
                  | <assign-stmt>
                  | <expr> ';' ;                               -- parse_stmt (LET_STMT, TRUNCATE_STMT, DELETE_STMT, INSERT_STMT, UPDATE_STMT, ASSIGN_STMT, or EXPR_STMT)
<assign-stmt>   ::= <expr> ':=' <expr> ';' ;                   -- parse_stmt (ASSIGN_STMT)
                    -- Relational assignment. The parser accepts any
                    -- expression as the target (LHS); the typechecker
                    -- restricts it to a name bound to an assignable relvar
                    -- (public or private; T0033 otherwise). A public target
                    -- is a write to its SQL table — the RHS shape is recognized
                    -- and emitted as surgical DML at lowering.
<truncate-stmt> ::= 'truncate' <expr> ';' ;                    -- parse_truncate_stmt
                    -- Clear every tuple from a relvar — sugar for the
                    -- relational assignment `R := R minus R` (the surgical
                    -- whole-table delete shape). The operand is parsed as an
                    -- <expr> (P0014 if absent); the typechecker restricts it
                    -- to a bare assignable relvar name (T0033) and requires a
                    -- transaction for a public relvar (T0025). `truncate` is a
                    -- contextual keyword recognized only as the leading token
                    -- of a statement (the `let` precedent) — it stays a usable
                    -- identifier everywhere else (no reserved words).
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
<tuple-set>     ::= '{' [ <tuple-lit> { ',' <tuple-lit> } [ ',' ] ] '}' ;
                                                               -- parse_tuple_set
                    -- A brace tuple-set — the keyword-less spelling of a
                    -- relation literal. It builds the same RELATION_LIT node
                    -- (the body is identical to <relation-lit>'s and reuses its
                    -- tuple-body codes P0032 / P0033), so the checker and
                    -- lowerer treat it as a relation source uniformly. An empty
                    -- `{}` is a zero-tuple relation literal (rejected, T0018).
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
                    '=' <expr> ';' ;                           -- parse_let_stmt
<for-stmt>      ::= 'for' <identifier> ':=' <expr> 'to' <expr>
                    'do' <block> ';' ;                         -- parse_for_stmt (FOR_STMT)
                    -- A counted loop with an INCLUSIVE upper bound (`i <= hi`).
                    -- `to` and `do` are contextual keywords recognized only in
                    -- this statement position (the `let` precedent); each bound
                    -- is a full <expr> that stops at the next keyword because
                    -- neither `to` nor `do` is an infix operator or a postfix
                    -- trigger. The counter is loop-scoped, `Integer`, and
                    -- immutable — assigning it is T0072; both bounds must be
                    -- Integer (T0071). `lo > hi` runs zero times (empty-safe at
                    -- the header test). A trailing `;` is required — a `for` is
                    -- a statement, never a value.
                    -- P0062 on a missing counter name, P0063 on a missing `:=`,
                    -- P0064 on a missing `to`, P0065 on a missing `do`, P0066 on
                    -- a missing body `[`; P0013 on a missing trailing `;`.

<expr>          ::= <expr-prec> ;                            -- parse_expr
<expr-prec>     ::= <primary-expr> { <postfix> }
                    { <infix-op> <expr-prec> | <project-suffix>
                      | <replace-suffix> | <tclose-suffix>
                      | <extend-suffix> | <rename-suffix>
                      | <wrap-suffix> | <unwrap-suffix> } ;
                                                               -- parse_expr_prec
                    -- Pratt precedence ladder; left-associative.
                    -- min_prec drives which operators may be
                    -- consumed; the parser recurses with `prec + 1`
                    -- for each rhs. The <project-suffix> / <replace-suffix>
                    -- / <tclose-suffix> / <extend-suffix> / <rename-suffix> /
                    -- <wrap-suffix> / <unwrap-suffix> postfix forms are
                    -- consumed only at pipeline level (min_prec 0),
                    -- interleaved with infix ops, so they bind to the whole
                    -- pipeline.
<infix-op>      ::= 'where'                                    -- prec 0
                  | 'join'                                     -- prec 0
                  | 'times'                                    -- prec 0
                  | 'compose'                                  -- prec 0
                  | 'intersect'                                -- prec 0
                  | 'union'                                    -- prec 0
                  | 'minus'                                    -- prec 0
                  | 'or'                                       -- prec 1
                  | 'and'                                      -- prec 2
                  | '=' | '<>' | '<' | '>' | '<=' | '>='       -- prec 3
                  | '+' | '-' | '||'                           -- prec 4
                  | '*' | '/' ;                                -- prec 5
                    -- `where`, `and`, `or` are contextual
                    -- keywords; the symbolic forms are token kinds
                    -- already lexed (Eq, Lt, Gt, LtEq, GtEq,
                    -- NotEq, Plus, Minus, Star, Slash, PipePipe).
                    -- Arithmetic binds tighter than comparison:
                    -- additive `+`/`-` and concatenation `||` at
                    -- prec 4, multiplicative `*`/`/` at prec 5.
                    -- `||` shares prec 4 with `+`/`-`; its rank
                    -- there is immaterial since its operands
                    -- (Text/Character) never mix with arithmetic.
                    -- The relational ops `join`/`times`/`compose`/
                    -- `intersect`/`union`/`minus` are also contextual
                    -- keywords. (Symbolic `-` is `Sub`; the keyword
                    -- `minus` is the relational set-difference op.)
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
                  | <paren-expr> ;                             -- parse_primary_expr
<bool-lit>      ::= 'true' | 'false' ;                         -- BOOL_LITERAL
<extract-expr>  ::= 'extract' <expr-prec> ;                    -- parse_extract_expr
                    -- TTM RM Pre 10 cardinality-checked
                    -- relation-to-tuple primitive. Wraps in
                    -- UNARY_EXPR. The operand parses at the
                    -- lowest precedence so `extract R where p`
                    -- reads as `extract (R where p)` without
                    -- parens.
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
                    -- a valid identifier elsewhere (no reserved words).
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
                    -- elsewhere (no reserved words).
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
                    -- identifier elsewhere (no reserved words).
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
                    -- keyword (no reserved words).
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
<transaction-expr> ::= 'transaction' <block> ;                 -- parse_transaction_expr
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
<relation-lit>  ::= 'Relation' '{' [ <tuple-lit> commalist ] '}' ;  -- parse_relation_lit
                    -- 'Relation' is a contextual keyword; recognized
                    -- by name in primary-expr position. The body is
                    -- a comma-separated list of tuple literals,
                    -- trailing comma allowed. Empty `Relation {}`
                    -- parses cleanly but typechecks as T0018 (no
                    -- inference context for the heading).
<sequence-lit>  ::= 'Sequence' '[' [ <expr> commalist ] ']' ;     -- parse_sequence_lit
                    -- 'Sequence' is a contextual keyword; recognized
                    -- by name in primary-expr position. The body is a
                    -- comma-separated list of element expressions,
                    -- trailing comma allowed (P0055 on a missing `[`,
                    -- P0056 if unterminated). Empty `Sequence []`
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
identifiers. Coddl has no hard-reserved words — see the "Reserved
words: none" section above.

### Deliberately not yet in the grammar

The following are decided design intent (see the rationale section
above) but not yet wired into the parser. Listed here so the omission
is explicit, not implied:

- **Tuple/Relation as a `<type-ref>`**. Today built-in scalar names
  and the `Sequence T` generator (see `<type-ref>` above) resolve as
  type references; the `Tuple H` / `Relation H` generator applications
  land alongside user-defined types.
- **Type generators** in `<type-ref>` — `Tuple H`, `Relation H`
  (`Sequence T` is now supported).
- **Statement forms** other than `<let-stmt>`, `<assign-stmt>`,
  `<truncate-stmt>`, `<delete-stmt>`, `<insert-stmt>`, `<update-stmt>`,
  and `<expr> ';'` — `mut`, `return`.
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
| P0018 | `let` statement is malformed (missing name, `=`, or RHS)|
| P0019 | `transaction` not followed by `[`                       |
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
| P0031 | Expected `{` after `Relation`                           |
| P0032 | Expected `{` to start a tuple in a relation literal     |
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
| P0055 | Expected `[` after `Sequence`                           |
| P0056 | Expected `]` to close sequence literal                  |
| P0057 | Expected `]` to close index expression                  |
| P0058 | Expected index expression                               |
| P0059 | Expected `then` after the `if` condition                |
| P0060 | Expected `[` to start the `if` block                    |
| P0061 | Expected `[` after `else`                               |
| P0062 | Expected a loop variable name after `for`               |
| P0063 | Expected `:=` after the `for` loop variable             |
| P0064 | Expected `to` after the `for` lower bound               |
| P0065 | Expected `do` after the `for` upper bound               |
| P0066 | Expected `[` to start the `for` loop body               |

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
