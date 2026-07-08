# Prelude — the builtin surface, in Coddl source

The prelude is a Coddl source file that **declares the built-in operators' signatures** in Coddl's own
syntax, using a leading `builtin` qualifier for operators the compiler implements rather than a `[ … ]` body:

```
builtin oper to_text { self: Integer } -> Text;
```

It replaces the hand-written Rust signature table ([`crates/coddl-types/src/builtins.rs`](../crates/coddl-types/src/builtins.rs))
as the source of truth for builtin *signatures*: the typechecker parses the prelude with the real parser,
builds its `OperSig` registry from the result, and the user reads the exact declarations the compiler
enforces. This is the `external` / `.d.ts` / prelude pattern (OCaml `external`, C headers, TypeScript
`.d.ts`, the Rust/Haskell preludes).

> **Status.** *Live.* The prelude source is [`coddl::core`](../crates/coddl-stdlib/modules/coddl/core.cd),
> embedded in the [`coddl-stdlib`](workspace.md) crate. Its `builtin oper` signatures are resolved via
> `coddl_stdlib::resolve` and loaded at typechecker construction (`Builtins::new`); `coddl-stdlib` embeds the
> source via `include_str!` so the compiler stays self-contained. `coddl-types` depends on `coddl-stdlib`
> (never the reverse — a cycle): the stdlib hands back source text, the typechecker interprets it.

**Last sync:** `31d3e74`. Every builtin-set or grammar change that touches the prelude surface updates this
doc and `coddl::core` in the same commit.

## Why do this

Builtins were a hand-written Rust table (`Builtins::new()` registering `OperSig` literals). Moving the
*signatures* into Coddl source buys three things a Rust table can't, all of which the project already values:

- **Single source of truth, no drift.** The signature the user reads is the signature the compiler parses and
  enforces — one artifact, in the language's own syntax, dogfooding the parser and typechecker.
- **LSP for free.** Go-to-definition / hover on a builtin lands in a real (virtual) Coddl document — the
  payoff of the frontend-serves-both-CLI-and-LSP discipline ([lsp.md](lsp.md)).
- **A self-hosting seam.** The prelude's *surface* is Coddl; `builtin` marks exactly the FFI-bottom line of
  [principles.md](principles.md) ("will stay Rust"). Every `builtin` oper is a later candidate to shed the
  marker and grow a real Coddl body.

## The `builtin` qualifier

`builtin oper NAME { params } -> Ret;` — the keyword `builtin` **leads** the declaration, qualifying the
`oper` as compiler-provided (no `[ … ]` body). A unit-returning builtin omits the return clause
(`builtin oper write_line { message: Text };`). Leading mirrors Coddl's existing declaration qualifiers —
`public` / `private` / `base` / `virtual` relvars — so `builtin` slots into the parser's item dispatch as one
more leading qualifier rather than a special body form, and both parser and reader learn "no body coming"
from the first token. Per "no reserved words" ([grammar.md](grammar.md)), `builtin` carries meaning only as a
leading item qualifier; it stays an ordinary identifier everywhere else.

**One surface marker, two lowering strategies underneath.** Some builtins are runtime FFI calls
(`to_text` → `coddl_int_to_text`); others are inline intrinsics. The user shouldn't care which, so the surface
has one keyword and the compiler keeps the name → lowering-strategy map (where `BUILTIN_EXTERNS` already
lives). The ABI symbol never appears in the prelude.

## What the prelude carries — and what it deliberately doesn't

The prelude is the **signature** source: parameter headings + return types. It is *not* the full builtin
metadata source. Three things stay compiler-side, keyed by the operator name:

- **Purity** (`Pure` vs `SideEffecting`, which gates use inside a `transaction`). There is no surface syntax
  for it yet, and it is an implementation fact, not a signature fact.
- **The lowering strategy** (runtime symbol vs inline intrinsic).
- **The codegen handler.**

And three builtin surfaces cannot be expressed in Coddl today, so they remain wholly compiler-registered:

- **`format`** — a variadic `FormatText`-template intrinsic the checker special-cases; it has no ordinary
  signature (see [typecheck.md](typecheck.md)).
- **`write_relation` and `cardinality`** — heading/element-polymorphic (`rel: Relation H`,
  `self: Relation H | Sequence T`). Heading polymorphism has no surface syntax; it is deferred
  ([risks.md](risks.md), "Heading polymorphism design space"). `parse_type_ref` requires a concrete
  `{ heading }` after `Relation`/`Tuple`, so there is no way to write "a relation of any heading" yet.
- **The infix / symbolic operators** (`+ - * / div mod = <> < > <= >= join times intersect union minus
  compose where …`) — recognized directly by the parser and checker, not called as `name { }`, so they are
  not `oper` declarations at all.

## Layering (why there is no bootstrap paradox)

- **L0 — compiler-intrinsic:** the primitive *types* and literals (`Integer`, `Text`, `Character`, `Boolean`,
  `Rational`, `Approximate`, `Tuple`, `Relation`, `Sequence`), known via `Type::from_builtin_name`. They back
  literals and carry special representation, so the prelude *references* them but does not define them.
- **L1 — the prelude (Coddl source):** builtin operator signatures over L0.
- **L2 — user code.**

## Type declarations

The `type Name = <type-ref>;` production (landed alongside the prelude) names a structural type — an alias
the typechecker resolves wherever the name is used:

```
type Pair = Tuple { a: Integer, b: Integer };
```

The prelude itself declares no types today; this is the mechanism a future standard library (or user code)
uses to name composite types. Shadowing a built-in type name is rejected (T0085), as is a duplicate
declaration (T0086). The exact scope of type declarations — alias-only, or a future home for
scalar-type/possrep declarations — is an open design question.

## The binding, relocated

Replacing the Rust signature table does not remove the compiler ↔ builtin binding; it *relocates* it, from
"Rust `OperSig` table vs. reality" to "prelude `builtin` decl vs. the codegen handler (+ purity) for that
name." That is a smaller, closed, checkable set: a startup assertion that every `builtin` oper in the prelude
has a codegen handler and a purity entry, and every codegen builtin has a prelude signature. Drift becomes a
build error instead of a silent lie. (This closed-set check is not yet wired — see below.)

## Modules

The core conversions / `to_text` / arithmetic are `coddl::core`, always in scope; library-specific surfaces
live in **opt-in** modules a file brings in with `use module <path>;`. Live today:

- **`coddl::web`** — the `Request` / `Response` vocabulary the web host marshals across the C ABI
  ([webhost.md](webhost.md)). A CLI program that never imports it does not have `Request` in scope.
- **`coddl::env`** — the process environment as a `builtin relvar Environment { name: Text, value: Text }`:
  a *new relvar kind* whose backing the runtime supplies via FFI. Read as any relation
  (`Environment where name = …`, via `coddl_env_snapshot`); written with the ordinary relvar DML —
  `insert`→`setenv`, `update`→`setenv`, `delete`→`unsetenv` (`coddl_env_insert` / `coddl_env_unset`). The
  general `R := …` surgical form is not yet wired for builtin relvars (T0033).

`coddl::` is a closed, compiler-owned, embedded root; module sources live in `coddl-stdlib`
([workspace.md](workspace.md)). Opt-in names are registered **lazily** — only in a file that `use`s their
module — so an un-imported stdlib name (`Request`) stays a free identifier the user may define
themselves (no reserved words, [grammar.md](grammar.md)). Referencing one without the import is not a plain
unknown-name error but an actionable **T0087** (operator) / **T0088** (type) pointing at the missing
`use module`; an unknown module path is **T0089**. Imports are **bring-bare-names**: `::` is a module-path
separator only, never used in expression or type position, so after `use module coddl::web;` you write
`Request`, not `web::Request`. See [grammar.md](grammar.md) for the `use module` / `::` grammar.

## What's here, and what's deferred

Landed: the `builtin` qualifier grammar, the loader (signatures parsed from `coddl::core`), and the `type`
alias production. Still open:

- **Quiet-path alias resolution** — the free `resolve_type_ref_quiet` (user-oper pre-pass, ProcIR lowerer)
  does not consult type aliases yet, so an alias used as a user-oper *param* type resolves quietly to
  `Unknown`; the loud path (the common one) works.
- **The closed-set check** — every `builtin` oper ↔ a codegen handler + a purity entry.
- **LSP virtual-document exposure** — go-to-definition / hover landing *in* the prelude.
