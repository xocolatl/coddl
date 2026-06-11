# Coddl grammar

This document is the authoritative spec for Coddl's surface syntax —
the precise form of the language the parser currently accepts.

For *why* the rules look the way they do, see `ARCHITECTURE.md §3
"Conformance to the Third Manifesto"` (surface syntax, brackets vs
braces, identifier case, reserved-words-none, Unicode glyph synonyms,
literals, method-style calls). This document never duplicates that
rationale — it points at it and gets on with the rules.

**Last sync:** `cb0b5c7` (AST view types). Every commit that adds,
removes, or changes a production, token, or diagnostic code updates
this file in the same commit; `tools/check-grammar.sh` enforces it
from the hygiene gate.


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
                  | <oper-decl>
                  | <unknown-item> ;                          -- parse_item

<program-decl>  ::= 'program' <identifier> ';' ;              -- parse_program_decl

<oper-decl>     ::= 'oper' <identifier> <heading>
                    <block> ';' ;                              -- parse_oper_decl

<heading>       ::= '{' [ <param> commalist ] '}' ;            -- parse_heading
<param>         ::= <identifier> ':' <type-ref> ;              -- parse_param
<type-ref>      ::= <identifier> ;                             -- parse_type_ref

<block>         ::= '[' { <stmt> } ']' ;                       -- parse_block
<stmt>          ::= <expr> ';' ;                               -- parse_stmt (EXPR_STMT)

<expr>          ::= <primary-expr> { <arg-list> } ;            -- parse_expr
<primary-expr>  ::= <name-ref>
                  | <literal> ;                                -- parse_primary_expr
<name-ref>      ::= <identifier> ;
<arg-list>      ::= '{' [ <named-arg> commalist ] '}' ;        -- parse_arg_list
<named-arg>     ::= <identifier> ':' <expr> ;                  -- parse_named_arg

<unknown-item>  ::= -- error recovery: any tokens until the next
                    -- top-level ';' at bracket-depth zero or EOF.
                    -- Emitted as PARSE_ERROR with diagnostic P0001.
                    ;                                          -- parse_unknown_item
```

`program` and `oper` are **contextual keywords** — the parser
identifies them by lexeme at specific syntactic positions; outside
those positions they are regular identifiers. Coddl has no
hard-reserved words. See `ARCHITECTURE.md §3 "Reserved words: none"`.

### Deliberately not yet in the grammar

The following are decided in `ARCHITECTURE.md` but not yet wired into
the parser. Listed here so the omission is explicit, not implied:

- **Return-type clause** on `<oper-decl>`. The punctuation choice
  (`:` vs `->`) is open. An `oper` without a return clause implicitly
  returns `Tuple {}` (the unit type).
- **Type generators** in `<type-ref>` — `Tuple H`, `Relation H`,
  `Sequence T`.
- **Infix operators** — relational (`join`, `times`, `intersect`,
  `union`, `minus`, `where`), comparison (`=`, `<>`, `<`, `>`, `<=`,
  `>=` polymorphic over scalars and relations), logical (`and`, `or`),
  arithmetic (`+`, `-`, `*`, `/`, `mod`).
- **Statement forms** other than `<expr> ';'` — `let`, `mut`, `return`,
  `insert`, `delete`, `update`.
- **Type / relvar / constraint declarations** at the top level.
- **Literals**: tuple `{ … }` and sequence `[ … ]` in expression
  position; relation literals `Relation { … }`.
- **Field access** (`x.y`) and **indexing** (`s[i]`) in expression
  postfix position.
- **Pattern matching**, **`if`/`else`**, **block-tail expressions**,
  **anonymous opers**.


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
