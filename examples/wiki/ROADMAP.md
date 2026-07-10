# Coddl Wiki — roadmap for a web app + a web framework

> **This document is the durable plan of record for building three things at once:**
> **(1)** a wiki web app, **(2)** a reusable web framework it's extracted from, and
> **(3)** the compiler/host/runtime features they force. It is written to survive a
> full context clearing: a fresh session with zero conversation memory should be able
> to read §0–§2, re-verify ground truth, find the next actionable item, and continue.

---

## 0. How to use this document (read first)

**To resume work from a cold start:**

1. Read §1 (orientation), §3 (settled decisions — don't relitigate), §4 (open decisions).
2. **Re-verify §2 ground truth against the current tree** — the codebase moves; the
   snapshot below may be stale. Each ground-truth claim says how to re-check it.
3. In §5, find the first `[ ] TODO` item whose dependencies are all `[x] DONE`. That's
   the actionable frontier. If several are ready, prefer the lowest phase number.
4. Implement it. Its **Acceptance** line says how you know it's done.
5. **Mark it `[x] DONE (commit <hash>)` and update this file in the same commit.** Move
   any newly-unblocked items forward. If you made a decision in §4, record it there.

**Status markers** (used throughout §5):

- `[ ] TODO` — not started.
- `[~] WIP` — in progress (say who/what in a note).
- `[x] DONE (commit <hash>)` — landed and verified; cite the commit.
- `[!] BLOCKED` — cannot start; the blocker is in its `Depends on` line.
- Decisions use `[ ] OPEN` / `[x] DECIDED: <choice> — <why>`.

**Layer tags** (which stratum an item belongs to — routes work to the right stratum per
the self-hosting fault line, §1):

- `LANG` — compiler/language feature (in `crates/`). Changes the language surface.
- `HOST` — Rust host / runtime (`crates/coddl-web`, `crates/coddl-runtime`). Transport, bytes.
- `FW` — userspace **Coddl** framework code (app-agnostic `.cd`). **Never** stdlib.
- `APP` — wiki-specific Coddl (`.cd`) + data.

---

## 1. Orientation (for a fresh reader)

We are building, in one interleaved effort:

- **The app** — a wiki (view pages, list pages, edit/create pages, later revisions +
  wikilinks). App-specific Coddl: relvars + queries + HTML.
- **The framework** — the app-agnostic request→dispatch→response plumbing (router,
  path-param extraction, response builders, HTML escaping, form parsing, templating),
  written as **userspace Coddl** and *extracted from the app as patterns repeat*. It is
  **not** built speculatively and **not** part of the stdlib.
- **The supporting compiler/host/runtime work** the wiki forces (Text primitives,
  userspace module imports, public-relvar writes through the web lifecycle, …).

**Method (non-negotiable): the app leads, the framework is extracted.** Write a slice of
the wiki; when it hits a wall, add the minimum at the correct layer and factor the
reusable shape into the framework. Do not design framework surface (middleware, template
DSLs, config systems, plugin loaders) before the wiki has demonstrably needed it twice.
This is the project's "promote proven idioms **down**; don't speculate a grab-bag API
**up**" rule (see `docs/principles.md`, `docs/webhost.md` "Design note").

**The self-hosting fault line** (routes each item to a layer — `docs/principles.md`):
sockets, byte parsing, HTTP framing, and (for now) percent-decoding stay **Rust (HOST)**;
the relational middle — models, queries, handlers, routing, templating — is **Coddl
(FW/APP)**. "Staying in Coddl" (pulling the socket loop into Coddl) is a long-term
direction, out of scope here.

**Authoritative docs** (read the relevant one before changing its area):
- `docs/webhost.md` — the web host, the C ABI, the request/response contract, the
  "vocabulary not framework" design note, routing-as-userspace, deferred items.
- `docs/principles.md`, `docs/conformance.md` — the non-negotiables (perf, long-term
  planning, TTM conformance, **no nulls ever** RM Pro 4).
- `docs/grammar.md`, `docs/typecheck.md`, `docs/relir.md`, `docs/sqlemit.md`,
  `docs/procir.md`, `docs/runtime.md`, `docs/storage.md`, `docs/plan.md`.
- `examples/web-users/` — **the working template** this wiki is modeled on (P4 payoff: a
  mainless handler queries a public relvar pushed to SQL and serves the result over HTTP).

---

## 2. Ground truth (RE-VERIFY — the tree moves)

Snapshot as of the P4 commit (`55b7500`, "P4: relvar-backed web handler serving SQL query
results over HTTP"); tree tip is now `33e68b9` (mandatory file-kind headers landed since).
**Before trusting any line, re-check it** — instructions inline.

- **P4 works end-to-end.** A mainless `oper handle { _req: RawRequest } -> RawResponse`
  queries a public relvar pushed to SQL, `load`s the result, loops to build a `text/plain`
  body, and serves it via `coddl-web` + curl. *Re-verify:* `examples/web-users/handle.cd`
  exists; `sh examples/web-users/seed-db.sh && coddl emit-obj … && CODDL_APP_OBJ=… cargo run
  -p coddl-web` then `curl localhost:8000/`.
- **A0 wiki scaffold exists (this roadmap's first APP item).** `examples/wiki/` mirrors
  `examples/web-users/`: `wiki.cd` (a `library`; `public relvar Pages { slug, title, body }
  key { slug }`), `wiki.cddb`, `wiki.cdstore`, `seed-db.sh`, `.gitignore`. The `handle`
  ignores the path and serves the hardcoded `home` page as `text/html` (raw, unescaped —
  escaping waits on F4/L1). *Re-verify:* the 5 files exist; `sh examples/wiki/seed-db.sh &&
  cargo run -p coddl-driver -- emit-obj examples/wiki/wiki.cd -o /tmp/wiki.o && CODDL_APP_OBJ=/tmp/wiki.o
  CODDL_WIKI_FILE=$PWD/examples/wiki/wiki.sqlite cargo run -p coddl-web`, then `curl -si localhost:8000/`.
- **Text primitives are ABSENT.** The builtin registry has only `to_text` (plus
  `to_rational`/`to_approximate`). No `split`/`substring`/`index_of`/`contains`/
  `starts_with`/`replace`/`length`. String tools today: `||` concat, `s[i]` indexing,
  `cardinality`, `format`/`f"…"` interpolation. *Re-verify:* grep string-op names in
  `crates/coddl-types/src/builtins.rs` and `crates/coddl-stdlib/modules/coddl/core.cd`.
- **Module resolution is STDLIB-ONLY.** `use module` resolves against the embedded
  `coddl-stdlib` crate (`coddl_stdlib::resolve(ModulePath::parse("coddl::web"))`,
  `crates/coddl-types/src/builtins.rs:~104`). No userspace/local `.cd`-imports-`.cd` path.
  *Re-verify:* grep `resolve`/`ModulePath`/`use module` in `crates/coddl-types/src/` and
  `crates/coddl-stdlib/src/`.
- **File-kind headers are now mandatory (`33e68b9`).** Every `.cd` opens with a
  `program` / `library` / `module` header (diagnostics PL0012–PL0015); a web-handler file is
  a `library` (or `module`), and lifecycle emission keys off that declared kind — see
  `docs/webhost.md` "Lifecycle". An L2 precursor, but module resolution is still stdlib-only.
  *Re-verify:* grep the header keywords + `PL001` in `crates/coddl-syntax/src/parser.rs`;
  check the header line in `examples/web-users/`.
- **Loop-carried owned `Text` lowers (T0076 partially lifted, `55b7500`).** `var body :=
  ""; for r in rows do [ body := body || … ]` is RC-correct on both backends. Still
  deferred (T0076 still fires): relation/sequence carries across loops, and any heap/`Text`
  carry across an `if` merge. *Re-verify:* `crates/coddl-procir/src/lower.rs`
  `carried_value_vars` (`allow_text` flag).
- **Bare-Boolean predicate pushdown works (`55b7500`).** `R where flag` ≡ `R where flag =
  true`, pushes as `WHERE "flag" = ?`. *Re-verify:* `build_predicate` in `lower.rs`.
- **Public-relvar WRITES: work in a `main` program, UNEXERCISED through the web lifecycle.**
  Surgical DML (`R := …` insert/update/delete/truncate) is committed and works for a
  writable public relvar (`WritePolicy::ReadWrite` when 1:1 on a base relvar — see
  `docs/storage.md` "Write policy", `crates/coddl-plan`). Whether a write inside a
  *mainless* handler's `transaction [...]`, driven by the synthesized `coddl_app_init`,
  works is **not yet tested** — same "synthesized-but-unexercised" risk class P4's read
  path was. *Re-verify:* `examples/insert-update-delete/`, and search e2e for a
  mainless/web write test (likely none yet).
- **Host: one `handle` symbol, `RawRequest`/`RawResponse` over the boxed C ABI.** `path`/
  `query` are **raw, percent-encoded** possrep scalars (`.value` is `Text`); `headers` is
  an `OrderedNameValues` relation; `body` is `Text`. Response headers flow back; framing
  headers host-owned. Handler linked via `CODDL_APP_OBJ` (build.rs). *Re-verify:*
  `crates/coddl-web/src/main.rs`, `crates/coddl-stdlib/modules/coddl/web.cd`.
- **Runtime is single-threaded** (global Mutex registries, one connection per db, atomic
  tx-depth). Concurrency is deferred. *Re-verify:* `docs/webhost.md` "What's deferred".

---

## 3. Settled decisions (do NOT relitigate)

- **The framework is userspace Coddl, never `coddl-stdlib`.** `coddl::web` stays two tiers
  (contract types + assumption-free primitives). Router/templating/forms/`Routes` are FW.
- **The host stays at ONE `handle` symbol forever.** Routing lives above the FFI line, in
  Coddl. The app's `handle` inspects the request and delegates.
- **The app leads; the framework is extracted.** No speculative framework surface (§1).
- **No nulls, ever (RM Pro 4).** Missing info → vertical decomposition (a side relvar), not
  a nullable column. "Page has no summary yet" = absent tuple in a `PageSummary` relvar.
- **`RawRequest` stays raw/percent-encoded.** Decoding is a separate, explicit step (its
  layer is decision D1).
- **Single self-contained binary** (`coddl-web` host + compiled handlers + relvars +
  runtime, statically linked); a reverse proxy (nginx) sits in front for TLS/static/LB.

---

## 4. Open decisions (make these as they become blocking)

- **D1 — Where does percent-decode + urlencoded parsing live?**
  `[x] DECIDED: HOST (provisional) — one Rust raw→cooked transform (H1), reversible to FW when L1 lands.`
  The host builds a **cooked `Request`** (decoded path + query) alongside the raw
  `RawRequest` (which stays raw, §3), so routing and path-param extraction become plain
  relational queries with **no L1 dependency** — A2 can route before Text primitives exist.
  Path decodes to `PathSegments = Relation { ordinality: Integer, segment: Text }` — a
  relation, **not** a `Sequence` (user call): `ordinality` is mandatory (else `/a/a` collapses
  under RM Pro 1) and it follows the `headers` precedent (`web.cd`: an ordinal attribute is
  data; stay queryable); `load` it into a sequence at point of use for ordered iteration.
  Full cooked shape + decode rules live in **H1**.
  - *Tradeoff (accepted):* this puts decode *policy* in the host, against `web.cd`'s "base
    types stay raw, decomposition is userspace" note — provisional and reversible: when L1
    exists the transform moves to FW and the host reverts to raw-only.
  - *Resolves:* F5's layer (HOST, reusing H1's decoder). *Unblocks:* H1 → F1, F2 (off L1).
- **D2 — Build userspace module imports (L2) early, or keep app+framework one file first?**
  `[ ] OPEN`
  - *Early:* the framework becomes a real separate module immediately; more upfront LANG
    work before the wiki renders a page.
  - *Late:* read-only wiki ships as one `.cd` with a conventional FW/APP seam; extract to a
    module once L2 lands. (Recommended default: **late** — ship pages first.)
- **D3 — Demo wiki or production wiki?** `[ ] OPEN` — a demo stays single-threaded (fine,
  concurrency deferred). A real multi-user wiki eventually forces L5 (runtime concurrency),
  a large separate arc. Sets how far Phase 6 matters.
- **D4 — When to extract the framework to its own repo?** `[ ] OPEN` — triggers: L1 + L2 +
  L3 all `DONE` and stable. Until then, in-tree. (See Phase 5.)

---

## 5. The roadmap

Legend: `[status] ID (LAYER) — title` · `Depends on:` · `Unblocks:` · `Acceptance:` · files.

### Phase P0 — Scaffold (prove the app skeleton on the P4 foundation)

- `[x] A0 (APP) — Wiki scaffold + models.` DONE (commit `0272695`) — `examples/wiki/`
  now mirrors `examples/web-users/` (`wiki.cd` + `wiki.cddb` + `wiki.cdstore` + `seed-db.sh`
  + `.gitignore`); a mainless `handle` serves the hardcoded `home` page as `text/html`
  (verified: `curl -si localhost:8000/` → `200`, `Content-Type: text/html`, `<h1>Home</h1>`).
  Env override is `CODDL_WIKI_FILE`. Original spec:
  Create `examples/wiki/` as a copy of the `examples/web-users/` shape: `wiki.cd`
  (opens with a mandatory `library wiki;` file-kind header (`33e68b9`); `use module coddl::web;`, `database wiki;`, `oper handle { _req: RawRequest }
  -> RawResponse`), `wiki.cddb`, `wiki.cdstore`, `seed-db.sh`, `.gitignore`. Model
  `public relvar Pages { slug: Text, title: Text, body: Text } key { slug };`. First
  `handle` ignores the path and serves ONE hardcoded page's HTML from a `Pages where slug
  = "home"` query (reuses P4 exactly: query → load → build HTML body → `text/html`).
  Depends on: — (P4 is done). Unblocks: A1, A2.
  Acceptance: `seed-db.sh` + `emit-obj` + `coddl-web` + `curl localhost:8000/` returns the
  home page's `<html>…</html>` with `Content-Type: text/html`.

- `[x] A-P4 (APP) — Reference template exists.` DONE (commit `55b7500`) — `examples/web-users/`
  is the working model for A0. Nothing to do; listed so a fresh reader finds the template.

### Phase P1 — Read-only wiki (the first recognizable wiki)

The keystone phase. Routing rides **H1** (a host transform — no L1); safe rendering
(F4/F6) still needs **L1 (Text primitives)**.

- `[ ] L1 (LANG) — Text primitives.`
  In-process string operations, designed to also **push to SQL** where the backend has a
  native function. Minimum viable set (scope precisely before starting; see D1 which may
  enlarge it): `length`, `substring`/slice, `index_of`, `contains`, `starts_with`,
  `ends_with`, `split` (→ `Sequence Text`), `replace`, `to_upper`/`to_lower`. Decide
  char-vs-byte indexing (UAX #31 / codepoints, consistent with the lexer). Register as
  builtins (see `to_text` precedent in `crates/coddl-types/src/builtins.rs`); lower in
  `crates/coddl-procir/src/lower.rs`; implement runtime entry points in
  `crates/coddl-runtime`; SQL pushdown mappings in `crates/coddl-sqlemit`. Update
  `docs/grammar.md`/`docs/typecheck.md`/`docs/sqlemit.md` in the same commit.
  Depends on: —. Unblocks: F4, F6, A5, and safe rendering (F1/F2/F5 now ride H1, not L1).
  Acceptance: unit tests per op (typecheck + lower + both backends); an e2e that splits a
  path and rebuilds a string in-process, leak-gated; a golden-SQL test showing one op
  pushing to SQLite. **This is the highest-leverage item in the whole roadmap.**

- `[ ] H1 (HOST) — raw→cooked Request transform.`
  One Rust function in `coddl-web` that turns the parsed wire request into a **cooked
  `Request`** value (D1), hand-marshalled like `RawRequest` (reuse `build_headers` + a
  `{ordinality, segment}` descriptor; `split_target` already exists). It builds:
  - `path: PathSegments` — strip one leading `/`, split the rest on literal `/`, decode each
    segment (`%XX` only, **no** `+`->space); `/` -> `{}`, `/wiki/` -> `{(0,"wiki"),(1,"")}`.
  - `query: OrderedNameValues` — split `&`, split each on the first `=`, `+`->space then
    `%XX` on both name and value; `ordinality` = index.
  - `headers: OrderedNameValues` — lowercase names; `method`/`body` pass through unchanged.
  Consider normalizing dot-segments (`.`/`..`) for path-traversal safety. Define `Request` +
  `PathSegments` in `coddl::web` (`web.cd`) and switch the handler param `RawRequest` ->
  `Request` (an ABI change — fine at this stage); `RawRequest` stays a type (§3) for a future
  raw-bytes need.
  Depends on: —. Unblocks: F1, F2 (both off L1), A2's routing; resolves F5's layer.
  Acceptance: a handler reads `req.path`/`req.query` as relations over curl —
  `/wiki/Home%20Page` yields a `path` tuple `(1, "Home Page")`; a golden test on the decode
  edge cases above (incl. split-before-decode: `/a%2Fb` -> one segment `"a/b"`, not two).

- `[ ] F3 (FW) — Response builders.`
  App-agnostic helpers that construct `RawResponse`: `html_response { status, body }`
  (sets `Content-Type: text/html`), `text_response`, `not_found {}` (404), `redirect { to }`
  (302 + `Location`). Pure `RawResponse`-tuple construction — needs no new language feature.
  Depends on: A0 (to have a handler to use them in). Unblocks: A2, A3.
  Acceptance: the wiki's `handle` builds all responses through these; the 404 path returns
  a real `404` over curl.

- `[ ] F4 (FW) — HTML escaping.`
  `escape_html { s: Text } -> Text` — `&<>"'` → entities, via L1 `replace`. XSS-critical:
  all user data (page titles/bodies) must pass through it before interpolation.
  Depends on: L1. Unblocks: F6, A2.
  Acceptance: a page whose title contains `<script>` renders escaped; a unit test asserts
  the entity output.

- `[ ] F1 (FW) — Router (method + path dispatch).`
  A `handle` helper that reads `req.method` and dispatches on `req.path` — the cooked
  `{ordinality, segment}` relation from H1. Dispatch is a **relational query**: e.g.
  `cardinality { req.path } = 0` for `/`, `req.path where ordinality = 0 and segment = "wiki"`
  for `/wiki/...` — so **no L1** (H1 already split + decoded the path). Start with a small
  explicit match; do NOT build a generic route table yet (that's F7). Keep it FW-tagged.
  Depends on: H1, F3. Unblocks: A2.
  Acceptance: GET `/` and GET `/wiki/{slug}` reach different sub-opers; an unknown path
  returns F3's `not_found`.

- `[ ] F2 (FW) — Path-param extraction.`
  Capture a segment from the cooked `req.path` relation (H1 already split + percent-decoded
  it). The slug at position 1 is `extract (req.path where ordinality = 1 project { segment })`
  — safe because F1's route guard guarantees exactly one tuple there (else `extract` errors
  on 0 rows; memory `extract-errors-on-empty`). No L1. (One thing to verify: `extract` over a
  relation-valued *tuple field* `req.path`, not just a relvar/query result.)
  Depends on: H1. Unblocks: A2, A3.
  Acceptance: `/wiki/Home` yields `slug = "Home"`; a unit/e2e test covers extraction.

- `[ ] F6 (FW) — Templating / HTML rendering helpers.`
  The body-building idiom from P4 (T0076 loop-carried `Text`) factored into reusable
  render helpers: render a list of tuples to HTML rows, wrap in a layout, always escape via
  F4. No template DSL — plain Coddl opers + `format`/`||`.
  Depends on: L1, F4. Unblocks: A2.
  Acceptance: a `Pages` query renders to an HTML `<ul>` of links, all values escaped.

- `[ ] A1 (APP) — Pages model + seed.`
  Finalize `Pages` relvar + `wiki.cddb`/`wiki.cdstore` + `seed-db.sh` with a few demo pages
  (Home, About). Mirror `examples/web-users/` exactly (Boolean→INTEGER lesson: get column
  SQL types right).
  Depends on: A0. Unblocks: A2.
  Acceptance: `seed-db.sh` produces `wiki.sqlite`; `emit-obj` clean; `check` clean.

- `[ ] A2 (APP) — Read-only routes.`
  GET `/` → list all pages (link each to `/wiki/{slug}`); GET `/wiki/{slug}` → render that
  page (`Pages where slug = <slug>`), 404 if absent (use `load`+`for` over the query, not
  `extract` — `extract` errors on 0 rows; see memory `extract-errors-on-empty`).
  Depends on: H1, F1, F2, F3, F4, F6, A1 (L1 still reached via F4/F6 for escaping/render). Unblocks: P2, A3.
  Acceptance: **a leak-gated e2e** (model on `examples/web-users` + the P4 hermetic test)
  asserting the index lists seeded pages and `/wiki/Home` renders Home; a manual curl flow
  documented in `wiki.cd`'s header. **This is "read-only wiki done."**

### Phase P2 — Make the framework real (physical seam)

- `[~] L2 (LANG) — Userspace module imports.`
  The file-kind-header foundation (`program`/`library`/`module`, PL0012–PL0015) landed
  (`33e68b9`); L2 proper — local module resolution + import + name mangling — remains.
  Let one userspace `.cd` import another (app `use`s a framework `.cd`) — today `use module`
  resolves only stdlib (§2). Design: local module path resolution in the driver + checker
  (`crates/coddl-types` module machinery, `crates/coddl-driver`), without privileging the
  framework as stdlib. Interacts with the `emit-obj`/single-compilation-unit model — decide
  whether it's multi-file-one-program or true separate modules.
  Depends on: — (independent LANG work; can proceed anytime). Unblocks: F-extract, D4.
  Acceptance: a two-file example where `app.cd` calls an oper defined in `lib.cd` via an
  import, compiles and runs; docs updated.

- `[ ] F-extract (FW) — Split framework into its own module.`
  Move F1–F6 out of `wiki.cd` into a framework module the wiki imports. First point where
  the FW/APP seam becomes physical (before L2 it was conventional).
  Depends on: L2, A2. Unblocks: cleaner P3+, D4.
  Acceptance: `wiki.cd` `use`s the framework module; behavior unchanged (A2's e2e still green).

### Phase P3 — Editable wiki (writes + forms)

- `[ ] L3 (LANG/integration) — Public-relvar writes through the web lifecycle.`
  Verify (and fix if needed) that `Pages := …` insert/update inside a *mainless* handler's
  `transaction [...]`, driven by synthesized `coddl_app_init`, actually writes to SQLite.
  DML works in `main` (§2); this is the web-lifecycle exercise, mirroring how P4 lit up the
  read path. May need a writable-relvar plan (`WritePolicy::ReadWrite`) through the
  emit-obj/app_init path.
  Depends on: — (can be verified independently; needed by A3). Unblocks: A3.
  Acceptance: a hermetic e2e — mainless handler inserts a row then reads it back through
  `app_init`/`app_shutdown`, leak-gated (model on `app_init_drives_a_mainless_public_relvar_sql_query`).

- `[ ] F5 (HOST) — Form-body parsing.`
  Parse `application/x-www-form-urlencoded` request bodies (`a=1&b=2`, `+`->space,
  percent-decode) into name/value pairs. Per **D1 = HOST**: a host helper reusing H1's query
  decoder, content-type-gated (only when `Content-Type` is form-urlencoded), surfaced as a
  `form: OrderedNameValues` field on the cooked `Request` (or a sibling host accessor).
  Depends on: H1. Unblocks: A3.
  Acceptance: a POST body `title=Hi&body=Hello%20world` parses to the right pairs; test.

- `[ ] A3 (APP) — Edit + create pages.`
  GET `/wiki/{slug}/edit` → an HTML form pre-filled from `Pages where slug=<slug>` (empty
  for a new page); POST `/wiki/{slug}` → parse the form (F5), write `Pages` (L3), redirect
  (F3) to the page. Guard against XSS on render (F4).
  Depends on: L3, F5, F2, F3. Unblocks: A4.
  Acceptance: a curl/e2e flow: POST a new page, then GET it and see the content; edit it,
  see the change. Leak-gated where hermetic.

### Phase P4 — Wiki-defining features

- `[ ] A4 (APP) — Revisions / history.`
  Vertical-decomposition model: `public relvar Revisions { slug: Text, revision: Integer,
  body: Text, edited_at: … } key { slug, revision };`. A save appends a revision; history
  view = `Revisions where slug=<s> order [desc revision]`. Pure Coddl-modeling payoff.
  Depends on: A3. Unblocks: —.
  Acceptance: editing a page twice yields two revisions; `/wiki/{slug}/history` lists them
  newest-first.

- `[ ] A5 (APP) — Wikilinks.`
  Rewrite `[[Page Name]]` in a body to `<a href="/wiki/Page%20Name">Page Name</a>` at render
  time, via L1 scan/replace. (Consider a `PageLinks` relvar later for backlinks — a graph
  query, Coddl's strength.)
  Depends on: L1, A2. Unblocks: —.
  Acceptance: a page body with `[[Home]]` renders a working link; test.

### Phase P5 — Extraction to its own repo

- `[ ] D4-extract (FW) — Spin the framework + wiki out to a separate repo.`
  Once L1+L2+L3 are stable (D4 triggers), move the framework (and the wiki as its example
  app) to a dedicated repo depending on a *released/pinned* compiler. This is the right home
  for a userspace framework (own cadence; proves external usability using only public
  features; not "blessed" as stdlib).
  Depends on: L1, L2, L3 (stable), D4 decided. Unblocks: —.
  Acceptance: the framework repo builds against a pinned `coddl` and its wiki example serves
  pages; this tree keeps (or drops) `examples/wiki/` as a thin smoke test per D4.

### Phase P6 — Someday (large, deferred)

- `[ ] L5 (LANG/HOST) — Runtime concurrency / re-entrancy.`
  The single-threaded runtime (global registries, one connection/path, atomic tx-depth) is
  the ceiling for a real multi-user wiki. A threaded/async per-request model is its own
  arc. Gated by D3.
  Depends on: everything above (only matters for a production wiki). Unblocks: real traffic.
  Acceptance: concurrent requests served correctly under a stress test; no shared-state races.

- `[ ] L4 (LANG) — `Binary` type` (byte bodies) — only if the wiki needs file uploads.
  Depends on: —. Unblocks: uploads. (Deferred; bodies stay `Text` until a concrete need.)

- `[ ] F7 (FW) — Routes-as-data.`
  A `Routes { name, method, pattern, handler }` relvar queried two ways (forward dispatch;
  reverse URL resolution). Ergonomic form needs L1 **and** first-class `oper` references
  (L6, a language feature — to *call* the handler a matched row names). Until then F1's
  explicit dispatch suffices. See `docs/webhost.md` "Routes are data".
  Depends on: L1, L6 (first-class oper refs — not yet scoped). Unblocks: data-driven routing.

---

## 6. Dependency graph (quick view)

```
H1 (host raw→cooked Request) ─┬─► F1 (router)      ─┐
                              └─► F2 (path params) ─┤
L1 (Text primitives) ─► F4 (escape) ─► F6 (render) ─┤
A0 (scaffold) ─► A1 (models) ──────────────────────┼─► A2 (read-only wiki)  ◄── keystone
F3 (responses) ────────────────────────────────────┘
A2 ─► [P2] L2 (userspace modules) ─► F-extract (physical FW seam)
A2 ─► [P3] L3 (public writes via web) ┐
        H1 ─► F5 (host form parse) ────┼─► A3 (edit/create) ─► A4 (revisions)
L1 ─────────────────────────────────► A5 (wikilinks)
L1+L2+L3 stable ─► D4-extract (own repo)
[P6] L5 (concurrency), L4 (Binary), F7+L6 (routes-as-data)  — deferred
```

**Critical path to a recognizable wiki:** routing via `H1 → (F1,F2)` (no L1) joined with
safe rendering via `L1 → (F4,F6)`, plus `F3` + `A1`, into `A2`. L1 remains the biggest single
unlock (escaping + rendering); routing no longer waits on it (D1 = HOST). The read path it all
rides on (query→SQL→HTML body) already works (P4).

---

## 7. Update protocol (keep this file true)

- When an item lands: flip its marker to `[x] DONE (commit <hash>)`, and in the same commit
  update any items it unblocks and the §2 ground-truth lines it changes.
- When you make an §4 decision: record `[x] DECIDED: <choice> — <why>` and adjust dependent
  items (e.g., D1 changes F5's layer).
- When you discover a **new** forced dependency (as P4 discovered T0076 and bare-Boolean
  pushdown): add it as a new `[ ] TODO` item with an ID, wire its `Depends on`/`Unblocks`,
  and note it in §2 if it changes ground truth.
- Keep IDs stable — other lines cross-reference them. Add new IDs; don't renumber.
- This file is the plan of record. If it disagrees with memory or a stale summary, **trust
  a fresh re-verification of the code (§2) over any remembered claim.**
