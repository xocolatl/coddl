# Routing design — data-driven routing + reverse URL resolution

> **Status: design captured, DEFERRED.** This is the fully-scoped design for the roadmap's
> **F7 (routes-as-data)** and the two language features it needs (**L6**, **L7**). It is
> *ahead* of the wiki's demonstrated need — the wiki routes `/` and `/wiki/{slug}`, which
> F1's explicit match covers. Per the app-leads rule (ROADMAP §1/§3), do **not** build this
> until an app actually needs data-driven routing or typed params. It is captured now only
> because the design thinking was done in a session; the worked sketch is embedded at the
> bottom (non-compilable — it uses surface the language doesn't have yet, each spot `[NEW]`).

## What it is

A route table queried **two ways**: forward (`method` + `path` → handler, run per request)
and reverse (`url_for(name, args)` → URL, for building links). TTM-clean throughout: no
nulls, no pointer attributes, everything is a relation or a value with decidable equality.

## Settled decisions

1. **Routes are stored vertically-decomposed** (not one fat relation):
   - `Routes        { name, method, handler }          key { name }`
   - `RouteLiterals { route, seg, part, text }         key { route, seg, part }`
   - `RouteParams   { route, seg, part, name, type }   key { route, seg, part }`
   - `OperParams    { oper, name, type }               key { oper, name }`  — the compiler's oper catalog

   The vertical split avoids a `kind` discriminator that would force nulls (RM Pro 4).
   Arity is **derived** (|distinct `seg` over literals ∪ params|); a trailing slash is just
   an empty-string literal — so there is **no** `RouteSegments` relation (don't store a
   derivable fact).

2. **`handler` is a module PATH (`Text`), not an operator value / pointer.** This resolves
   RM Pro 7 (no pointer attributes): the value's identity is its fully-qualified path,
   equality is path comparison (decidable), never a code address. The handler's *signature*
   lives in `OperParams` (joined by path). The "handler fits its route" rule is a
   **constraint** — a join + equality between `OperParams` and `RouteParams` — **not** the
   column's type, because the params vary per route (no single function type is the column
   type; a relation column has one type for all rows).

3. **First-class `oper` references (L6).** An `oper` reference value *is* its module path
   (decision 2). Written `oper path::name { sig } -> Ret` (the `oper` keyword disambiguates a
   signature from a call, since `name { … }` is call syntax). Direct call of a *value*:
   `(expr){ args }`, or bind first `let h = row.handler; h{ args }`. `x.name{}` stays UFCS —
   so parenthesize or bind to call an oper *value*. No `call` shim.

4. **Typed URL params, required, OPEN via a protocol (L7).** A type is usable as a
   `{name: T}` hole iff it provides `to_url_regexp{self: T} -> RegularExpression` (its match
   grammar). `Text`/`Integer` are builtin; a user opts their own scalar in (UFCS overload,
   exactly like `to_text`) — e.g. `PositiveInteger -> "[0-9]+"`. A URL-usable type is really a
   **triple**: `to_url_regexp` (match) + `from_url_text` (parse `Text→value`, the still-absent
   direction) + `to_text` (format, exists, used by reverse). `RegularExpression` is an ordinary
   `Text`-backed scalar (decidable equality — it does *not* reopen the operator-value problem);
   the regex **engine is a Rust HOST primitive**; the per-route regex is a **compile-time fold
   of the decomposed parts**, run per request.

5. **Forward matching = simulated relational division.** "For all a route's literals, the
   path agrees" is a universal → division, written `π − π(counterexamples)`: an **arity
   filter** (the completeness half) + a **literal anti-join** (the no-counterexample half).
   The compiled regex (L7) *subsumes* arity + division + composite-split for composite or
   typed segments. **Reverse never runs the regex** (a match-regex is one-directional) — it
   folds the parts the other way (literal→text, param→`to_text(args.name)`, %-encode, `/`-join).

6. **Captures are a transient heterogeneous TUPLE** (`{ slug: Text, id: PositiveInteger }`) —
   **not stored** (a relation column can't hold mixed types; a tuple heading can). Its heading
   is a *query* over `RouteParams`. The matcher parses once and hands the captures to the
   handler as named args — nobody re-parses.

7. **"Ambiguous route" = a forward-key violation, not a runtime accident.** Forward dispatch
   must be a function `(method, path) → handler` — a constraint on route *extents*. Overlaps
   resolve by **specificity** (literal > Integer > Text, leftmost-position dominant); genuine
   ties are a **compile-time reject**. There is *no* "first match wins by list order" — tuple
   order is RM Pro 1-forbidden; determinism comes from disjoint extents or derived specificity,
   never from position in a list.

## Language / host features this needs (roadmap items)

- **L1** — Text primitives (reverse assembly + %-encode; the `from_url_text` parse direction).
- **H1** — cooked `Request` with `req.path : PathSegments` (matching operates on it).
- **L6** — first-class `oper` references (module-path values + direct call).
- **L7** — typed URL param protocol: `to_url_regexp` type-class + `RegularExpression` scalar +
  a Rust regex-engine HOST primitive + `from_url_text` (Text→value parse).

## Open sub-questions (unresolved)

- **[SHADOW]** In `Pages where slug = slug`, the predicate scope injects the attribute `slug`,
  which shadows the capture param `slug` (reads as attribute=attribute). Disambiguating an
  outer capture from an injected attribute is unresolved.
- **Capture passing:** spread (`{req, slug, id}` — nice handlers, needs a tuple-splat) vs
  grouped (`{req, captures}` — uniform call, handlers read `captures.slug`).
- **`extract` over a relation-valued tuple field** (`req.path`) — verify it composes with
  `extract` (only exercised on relvars / query results so far).
- **Match strategy:** pure-relational division (simple segments, stays queryable) vs whole-route
  regex (handles composites, but per-route procedural). Either way the decomposed parts stay
  the source of truth; the regex is derived.

---

## The worked sketch (non-compilable design toy)

```coddl
library routing;

use module coddl::web; // RawRequest, RawResponse, RawRequestPath, RawRequestQuery, OrderedNameValues

// A DESIGN TOY — not compilable. It sketches the whole path from the ONE raw host handler
// down to typed per-route handlers. It leans on surface the language does NOT have yet;
// every such spot is marked [NEW]. See the decisions above for the rationale.

// ===== Cooked contract types (what handlers actually see) =====

type PathSegments = Relation { ordinality: Integer, segment: Text };

type Request = Tuple {
    method: Text,
    path: PathSegments,       // decoded, split-before-decode
    query: OrderedNameValues, // decoded pairs
    headers: OrderedNameValues,
    form: OrderedNameValues,  // parsed form fields (content-type-gated)
    body: Text,
};

type Response = Tuple { status: Integer, headers: OrderedNameValues, body: Text };

// ===== URL param types — the `to_url_regexp` protocol =====
// A type is URL-usable iff it has to_url_regexp (its match grammar). Triple: to_url_regexp
// (match) + from_url_text (parse, [NEW]) + to_text (format). RegularExpression is Text-backed;
// the engine is a Rust HOST primitive; the route regex is a compile-time fold of the parts.
// Grammars must be '/'-bounded (a segment can't contain '/'), so Text is [^/]+, not .*.

type RegularExpression { pattern: Text };

oper to_url_regexp{ self: Text }    -> RegularExpression [ RegularExpression{ pattern: "[^/]+" } ];
oper to_url_regexp{ self: Integer } -> RegularExpression [ RegularExpression{ pattern: "-?[0-9]+" } ];

type PositiveInteger { n: Integer }; // a user opts their OWN scalar in — the payoff of openness
oper to_url_regexp{ self: PositiveInteger } -> RegularExpression [ RegularExpression{ pattern: "[0-9]+" } ];

// ===== The route table, stored RELATIONALLY =====
// handler = module PATH (Text); captures are NOT stored (a transient tuple per request).

let Routes = Relation {
    { name: "home",      method: "GET",  handler: "routing::list_pages"   },
    { name: "wiki_page", method: "GET",  handler: "routing::show_page"    },
    { name: "article",   method: "GET",  handler: "routing::show_article" },
    // wiki_edit (GET), wiki_save (POST) elided
};

let RouteLiterals = Relation {
    { route: "wiki_page", seg: 0, part: 0, text: "wiki" },
    { route: "article",   seg: 0, part: 0, text: "news" },
    { route: "article",   seg: 1, part: 0, text: "articles" },
    { route: "article",   seg: 2, part: 1, text: "-" },   // delimiter INSIDE the composite seg
    { route: "article",   seg: 3, part: 0, text: "" },    // trailing slash = empty-string literal
    // "home" is "/" -> zero segments -> no rows
};

let RouteParams = Relation {
    { route: "wiki_page", seg: 1, part: 0, name: "slug", type: "Text"           },
    { route: "article",   seg: 2, part: 0, name: "slug", type: "Text"           },
    { route: "article",   seg: 2, part: 2, name: "id",   type: "PositiveInteger" },
};

// OperParams: the compiler's catalog (every oper's params). Routes.handler joins here.
// pattern<->handler agreement, per route R (a JOIN + equality):
//   (OperParams where oper = handler(R) and name <> "req") project {name, type}
//   == (RouteParams where route = R)                       project {name, type}

// ===== Forward dispatch (the joins) =====

oper dispatch{ req: Request } -> Response [
    // 1 - method filter
    let by_method = Routes where method = req.method;

    // 2 - arity filter (the completeness half of the division)
    let right_arity =
        by_method where cardinality {
            (RouteLiterals where route = name project { seg })
            union
            (RouteParams where route = name project { seg })
        } = cardinality { req.path };

    // 3 - literal match as SIMULATED DIVISION (for-all == no counterexample -> MINUS them).
    //     Composite/typed segments are matched instead by a compile-time-folded regex (§L7)
    //     that subsumes arity + division + split; reverse never runs it. [NEW]
    let bad =
        (RouteLiterals rename { seg: ordinality })
          join req.path
          where text <> segment
          project { route };
    let matched =
        right_arity minus (right_arity where (Relation { { route: name } } <= bad));

    // 4 - bind params -> transient capture TUPLE { slug: Text, id: PositiveInteger } (NOT a relation)
    // 5 - most-specific `matched` route, then direct first-class call [NEW]:
    //       out := (best.handler){ req, ...captures };   // spread; or grouped { req, captures }
    var out := not_found{};
    out
];

// url_for (reverse) folds the parts the OTHER way — it never runs the regex.
```
