# Coddl grammar

This document is the authoritative spec for Coddl's surface syntax —
the precise form of the language the parser currently accepts.

For *why* the rules look the way they do, see `ARCHITECTURE.md §3
"Conformance to the Third Manifesto"` (surface syntax, brackets vs
braces, identifier case, reserved-words-none, Unicode glyph synonyms,
literals, method-style calls). This document never duplicates that
rationale — it points at it and gets on with the rules.

**Last sync:** `1830ac1`. Every commit that adds, removes, or changes
a production, token, or diagnostic code updates this file in the
same commit; `tools/check-grammar.sh` enforces it from the hygiene
gate.


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

<key-clause>    ::= 'key' '{' [ <identifier> commalist ] '}' ; -- parse_key_clause
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
<expr-prec>     ::= <primary-expr> { <postfix> } { <infix-op> <expr-prec> } ;
                                                               -- parse_expr_prec
                    -- Pratt precedence ladder; left-associative.
                    -- min_prec drives which operators may be
                    -- consumed; the parser recurses with `prec + 1`
                    -- for each rhs.
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
                  | <relation-lit> ;                           -- parse_primary_expr
<bool-lit>      ::= 'true' | 'false' ;                         -- BOOL_LITERAL
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
identifiers. Coddl has no hard-reserved words. See
`ARCHITECTURE.md §3 "Reserved words: none"`.

### Deliberately not yet in the grammar

The following are decided in `ARCHITECTURE.md` but not yet wired into
the parser. Listed here so the omission is explicit, not implied:

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
