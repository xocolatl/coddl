# Grammar — surface syntax

The authoritative spec for Coddl's surface syntax — the precise form of the language the parser currently accepts, plus the design rationale behind every choice that doesn't immediately follow from TTM (see [conformance.md](conformance.md)).

This doc has two parts: **rationale** (the design decisions — why prefix-named-args, why no reserved words, why brackets-vs-braces, etc.) and the **productions** (the EBNF the parser implements, lexical and syntactic).

**Last sync:** `36f2239`. Every commit that adds, removes, or changes a production, token, or diagnostic code updates this file in the same commit; `tools/check-grammar.sh` enforces it from the hygiene gate.

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
- **Textual relational**: `join`, `times`, `intersect`, `union`, `minus`, `where`.
- **Textual logical**: `and`, `or`. (Both `Boolean × Boolean → Boolean`. `or` < `and` < `not` < comparison on the precedence ladder; final ordering deferred to the parser phase.)
- **Textual arithmetic**: `mod`. (`Integer × Integer → Integer`. Binds at multiplicative precedence, alongside `*` and `/`. Defined only for `Integer` in v1 — extending to `Rational`/`Approximate` needs a separate semantic decision.)

Reason: the named-prefix form is clumsy for ubiquitous dyadic ops on identifier-unfriendly names, and the textual binary ops all have natural infix readings from math and SQL. No-reserved-words still holds — `join` is recognized contextually in expression position; it remains a valid identifier elsewhere.

**`times` and `intersect` are typed aliases of `join`.** Both lower to the same `AND` node in Algebra A (see [relir.md](relir.md) — `AND` generalizes TIMES and INTERSECT). The aliases exist for intent-signaling and compile-time enforcement: `times` requires the two heading sets to be **disjoint** (otherwise the user meant `join`, not a cartesian product, and the type checker catches the slip); `intersect` requires the two heading sets to be **identical**. Both checks are static, both are zero-cost at runtime.

**`union` and `minus` require identical headings.** `union` lowers to A-core `OR` (heading-agnostic relational union, restricted at the type level to matching headings since Coddl has no nulls). `minus` lowers to `AND NOT` (set difference is `R join (NOT S)` when headings match). Both checks are static; mismatched-heading attempts are rejected at compile time with a diagnostic.

**`where` (restriction) is also infix and special-cased in two ways.** The right operand is a *predicate*, not another relation, and that has two consequences:

- **Scope injection.** Identifiers in the predicate resolve against the left operand's heading first, the enclosing scope second. `SP where s# = supplier` reads as: `s#` is the `SP` attribute; `supplier` is a parameter from the enclosing `oper`. The parser and typechecker inject the left operand's heading into the predicate's name-resolution scope automatically. This is the first construct with a non-uniform scoping rule; every later construct that takes a predicate (`extend`, `summarize`'s aggregate expressions, possrep constraints) reuses the same machinery.
- **Precedence.** `where` binds looser than `=`, `<`, `+`, `and`, `or` — the predicate is expected to be a full scalar expression. `R where x = 1 and y > 0` parses as `R where ((x = 1) and (y > 0))` without parentheses. Practically `where` sits at the bottom of the infix precedence ladder, alongside `union`/`minus`. Full precedence table lands when the parser does — exact order is deferred until then.

**`project` (projection) is a *postfix* operator, not infix.** `R project { a, b }` narrows the relation's heading to the named attributes — the right operand is a brace-list of bare attribute names (structurally identical to a `key { … }` clause), not another expression, so it doesn't fit the infix `<lhs> op <rhs>` shape. It is parsed as a postfix suffix at *pipeline precedence* — the same altitude as `where`, and gated to the top level so it binds to the whole pipeline rather than to a higher-precedence operand such as a `where` predicate. `R where p project { a }` reads as `(R where p) project { a }`; the reverse order `R project { a } where p` also parses, nesting left. It is the first postfix relational operator; later ones (`rename`, `extend`, `summarize`) can reuse the same pipeline slot. `project` remains a contextual keyword — a valid identifier everywhere else (no reserved words).

### Parenthesized positional for monadic operators

`count(R)`, `sin(x)`, `is_*(...)`, `not(p)`. Single argument, name-free, conventional shape.

### Named-prefix with braces for everything else

N-ary or structured operands: selectors, `oper` calls in general, `extend`, `summarize`, `rename`, `group`, `ungroup`, `wrap`, `unwrap`. These all have meaningful name slots that would be lost in a positional form.

This eliminates the relational-algebra/scalar-op syntactic distinction the authors regret, and matches RM Pro 1 (no ordinal-position semantics) at the surface where it's easiest to enforce.

## Brackets vs braces encode ordering

A consistent two-character distinction across the entire surface syntax:

- **`{ ... }` (curly braces) — unordered.** A set-like collection where position is meaningless. Used for named-argument lists, `Tuple` and `Relation` literals, heading declarations, and parameter lists in `oper` declarations. Reordering the contents preserves meaning.
- **`[ ... ]` (square brackets) — ordered.** A sequence where position is semantically significant. Used for `Sequence T` literals (`[1, 2, 3]`, `[tup1, tup2, tup3]`), operator bodies (statements run in order), `load` ordering specs, and any other context where the reader's expectation is "this is a sequence." Reordering changes meaning.
- **`( ... )` (parentheses)** — kept for expression grouping and for the small set of monadic operators that retain parenthesized positional form (`count`, `sin`, `is_*`).

This maps directly onto TTM: tuples, relations, and headings have no ordinal position semantics (RM Pro 1); they get `{ ... }`. Procedural code is sequential by nature; it gets `[ ... ]`. The punctuation tells the reader which kind of collection they're looking at without having to recall any context.

## Identifier case

Coddl is case-sensitive: `foo` and `Foo` are distinct identifiers. The language uses three case styles, applied consistently to built-ins and recommended for user code:

- **lowercase / snake_case** — keywords (`program`, `oper`, `where`, `join`, `load`, `if`, `then`, `else`, …), built-in operators (`and`, `or`, `not`, `count`, `sum`, `extend`, …), built-in constants (`true`, `false`, `reltrue`, `relfalse`), and user-named operators, variables, attributes, and parameters.
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
                        | <char-literal>
                        | <integer-literal>
                        | <rational-literal>
                        | <approximate-literal> ;

<string-literal>      ::= '"' { <string-char> } '"' ;
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
<type-ref>      ::= <identifier> ;                             -- parse_type_ref

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
                  | <expr> ';' ;                               -- parse_stmt (LET_STMT or EXPR_STMT)
<let-stmt>      ::= 'let' <identifier> [ ':' <type-ref> ]
                    '=' <expr> ';' ;                           -- parse_let_stmt

<expr>          ::= <expr-prec> ;                            -- parse_expr
<expr-prec>     ::= <primary-expr> { <postfix> }
                    { <infix-op> <expr-prec> | <project-suffix> } ;
                                                               -- parse_expr_prec
                    -- Pratt precedence ladder; left-associative.
                    -- min_prec drives which operators may be
                    -- consumed; the parser recurses with `prec + 1`
                    -- for each rhs. <project-suffix> is consumed only
                    -- at pipeline level (min_prec 0), interleaved with
                    -- infix ops, so it binds to the whole pipeline.
<infix-op>      ::= 'where'                                    -- prec 0
                  | 'or'                                       -- prec 1
                  | 'and'                                      -- prec 2
                  | '=' | '<>' | '<' | '>' | '<=' | '>=' ;     -- prec 3
                    -- `where`, `and`, `or` are contextual
                    -- keywords; the symbolic forms are token kinds
                    -- already lexed (Eq, Lt, Gt, LtEq, GtEq,
                    -- NotEq).
<postfix>       ::= <arg-list>                                 -- call: CALL_EXPR
                  | <field-access-tail> ;                      -- field access: FIELD_ACCESS
<field-access-tail> ::= '.' <identifier> ;
<primary-expr>  ::= <name-ref>
                  | <literal>
                  | <bool-lit>
                  | <transaction-expr>
                  | <tuple-lit>
                  | <relation-lit>
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
<transaction-expr> ::= 'transaction' <block> ;                 -- parse_transaction_expr
<name-ref>      ::= <identifier> ;
<arg-list>      ::= '{' [ <named-arg> commalist ] '}' ;        -- parse_arg_list
<named-arg>     ::= <identifier> ':' <expr> ;                  -- parse_named_arg
<tuple-lit>     ::= '{' [ <named-arg> commalist ] '}' ;        -- parse_tuple_lit
                    -- Same grammar as <arg-list>; the wrapping node
                    -- kind (TUPLE_LIT vs ARG_LIST) distinguishes a
                    -- tuple value from a call-site argument list.
                    -- Empty '{}' is the unit value, type Tuple {}.
<relation-lit>  ::= 'Relation' '{' [ <tuple-lit> commalist ] '}' ;  -- parse_relation_lit
                    -- 'Relation' is a contextual keyword; recognized
                    -- by name in primary-expr position. The body is
                    -- a comma-separated list of tuple literals,
                    -- trailing comma allowed. Empty `Relation {}`
                    -- parses cleanly but typechecks as T0018 (no
                    -- inference context for the heading).

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

- **Tuple/Relation/Sequence as a `<type-ref>`**. Today only built-in
  scalar names resolve as type references; type-generator
  applications (`Tuple H`, `Relation H`, `Sequence T`) land alongside
  user-defined types.
- **Type generators** in `<type-ref>` — `Tuple H`, `Relation H`,
  `Sequence T`.
- **Infix operators** — relational (`join`, `times`, `intersect`,
  `union`, `minus`, `where`), comparison (`=`, `<>`, `<`, `>`, `<=`,
  `>=` polymorphic over scalars and relations), logical (`and`, `or`),
  arithmetic (`+`, `-`, `*`, `/`, `mod`).
- **Statement forms** other than `<let-stmt>` and `<expr> ';'` —
  `mut`, `return`, `insert`, `delete`, `update`.
- **Type / relvar / constraint declarations** at the top level.
- **Literals**: sequence `[ … ]` in expression position. (Tuple
  `{ … }` literals and dot-prefix field access landed in Phase 18.
  Relation literals `Relation { … }` landed in Phase 19. Boolean
  literals `true` / `false` and infix `=`, `<>`, `<`, `>`, `<=`,
  `>=`, `and`, `or`, `where` landed in Phase 20.)
- **Indexing** (`s[i]`) in expression postfix position.
- **Arithmetic** (`+`, `-`, `*`, `/`, `mod`). Reserved precedence
  slot above comparison; not yet parsed.
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
| P0035 | Expected `)` to close parenthesized expression          |
| P0036 | Expected `{` to start project list                      |
| P0037 | Expected project attribute name                         |
| P0038 | Expected `}` to close project list                      |
| P0039 | Expected `but` after `all` in project                   |

Note: missing-type-after-`:` (let annotation) and missing-type-
after-`->` (operator return clause) both surface as `P0011`
("expected type name") via `parse_type_ref` — the diagnostic
message is identical and adding distinct codes would dedupe to the
same message.


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
