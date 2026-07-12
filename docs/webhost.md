# Web host — compiled Coddl behind an HTTP server

How a Coddl program serves HTTP requests, Django/FastAPI-shaped: a Rust **host** owns the socket loop,
HTTP parsing, routing, and JSON; Coddl provides the **models** (relvars) and the **handlers** (`oper`s),
compiled as a host-callable library. The host builds a request value, calls a handler, and serializes the
response it gets back.

> **Status.** *End-to-end: a relvar-backed handler serves SQL query results over HTTP.* The vocabulary this
> doc marshals — `Request` / `RawResponse` — is real: it lives in the opt-in [`coddl::web`](prelude.md)
> standard-library module, brought into scope with `use module coddl::web;`. **P1a, the Spine, P2, P3a, the
> RawRequest reshape, P1b, P4 (the payoff), and the cooked-`Request` transform (H1) have landed:** `coddl
> emit-obj` produces a mainless object, and the `coddl-web` crate is a single-threaded `TcpListener` that
> parses real HTTP requests (headers + `Content-Length` body), decodes the request-target and builds a
> **boxed cooked `Request`** value — `path`/`query` as percent-decoded relations with the raw parts kept
> alongside, passed as one pointer — calls the handler across the C ABI, reads a **boxed `RawResponse`** back,
> and writes the reply. A handler whose body queries a `public relvar` pushed
> to SQL, through the synthesized `coddl_app_init`/`coddl_app_shutdown` lifecycle, works end-to-end
> (`examples/web-users/`). What remains is routing (a **userspace Coddl framework**, not host work — see the
> design note below) and the deferred items at the end. It extends the first milestone: everything it relies
> on — the frontend, RelIR → SQL, ProcIR → native codegen, the `staticlib` runtime, user `oper`s with the
> C-ABI return convention — is assumed already end-to-end per [milestone.md](milestone.md).

**Last sync:** validated against `8ac70d9`; the `Request` / `Response` vocabulary has since landed as the
`coddl::web` module. Revalidate the file:line and ABI claims below when the entry model, the calling
convention, or the runtime lifecycle changes.

## The realization: you write handlers, not a server

In Django/FastAPI the *application developer* never writes the socket loop or the HTTP parser — uvicorn /
gunicorn (WSGI/ASGI) does. What they write is three things, and two of them are Coddl's home turf:

| Framework concept | Coddl equivalent | Where it lives |
|---|---|---|
| Pydantic / Django model | relvar + `Tuple` heading | relational middle |
| ORM query (`User.objects.filter(…)`) | a relational query, **pushed to SQL** ([sqlemit.md](sqlemit.md)) | relational middle |
| `@app.get("/users")` handler | `oper handle { req: Request } -> RawResponse` | relational middle |
| URL routing / dispatch (`urls.py`) | a userspace Coddl framework — `handle` delegates (see design note) | relational middle |
| uvicorn accept loop + HTTP parse + JSON | the **Rust host** (`coddl-web`) | FFI bottom |

The transport half being absent from Coddl is *correct*, not a gap — it isn't the app developer's job in
these frameworks either. It's **FFI-bottom "will stay Rust" work** by the self-hosting fault line in
[principles.md](principles.md): sockets, byte parsing, and JSON need the pointers and raw memory the surface
language deliberately forbids, so they stay in the host and never depend on the relational middle. The
ORM-equivalent half, by contrast, is a thing Coddl does *better* than the frameworks: a relvar query is the
model and the query at once, pushed to the backend, with no null (RM Pro 4) and no impedance mismatch.

The project already anticipates this shape: the runtime is a `staticlib` today, "`cdylib` later if plugin
loading lands" ([runtime.md](runtime.md), [workspace.md](workspace.md)). A web host is that same
embed-compiled-Coddl-as-a-library move, driven by a concrete application.

## Why a C ABI between host and handler — and what "staying in Coddl" would mean

The host calls the handler across a C ABI. It is worth being precise about what that *is*, because it sounds
heavier than it is and it raises the natural question: if both `coddl-web` and the handler are ours, why not
stay "in Coddl" and skip the ABI?

**The seam is a direct native call, not a boundary crossing.** There is no marshalling, serialization, or IPC
between host and handler — they link into one binary and the call is a single `call` instruction over an
agreed register/stack layout, plus the fat-pointer return convention ([codegen.md](codegen.md)) and a
`coddl_rc_release`. It is already in-process and zero-copy; the "C ABI" is the *calling convention*, not a
wire format.

**The convention exists because two different compilers meet here.** `rustc` builds `coddl-web`; Coddl's own
codegen (Cranelift / LLVM) builds the handler. They must agree on how arguments and returns are laid out, and
the only convention both toolchains implement is the C ABI. Coddl's codegen already emits C-linkage symbols
with no name mangling — this is the compiler↔runtime boundary [workspace.md](workspace.md) defines, not
something invented for the web host. `rustc` speaks it via `extern "C"`.

**You cannot make the host Coddl today, so *some* foreign boundary is unavoidable.** Sockets, `read`/`write`,
byte buffers, and (later) HTTP/JSON parsing need the raw pointers and mutable memory the surface language
forbids by design (the self-hosting fault line in [principles.md](principles.md)). That is why the transport
half being absent from Coddl is *correct, not a gap*. The only open question is *where* the foreign boundary
sits, not whether there is one.

**"Staying in Coddl" is a real long-term direction — but it moves the seam, it does not delete it.** The
self-hosting goal is to pull the socket *loop* itself into Coddl, leaving only an irreducible syscall shim as
FFI — `socket`/`accept`/`read`/`write` as builtin opers bottoming out in the runtime, exactly the pattern
`coddl::env` already uses (a builtin whose leaves are C-ABI calls — see [prelude.md](prelude.md)). Once the
loop is Coddl, the host→handler call becomes an *ordinary `oper` call*: same codegen, native value
representations, and **no hand-written `extern` declarations and no manual `coddl_rc_release`**, because the
compiler manages ownership on both sides. What that removes is the hand-written *marshalling ceremony* — not a
calling convention. Coddl-to-Coddl calls are themselves C-ABI-shaped today (unmangled C-linkage symbols; it is
why handler overloading is blocked on name mangling, see What's deferred). "In Coddl" means
*compiler-managed* marshalling rather than *hand-written*, not "no ABI."

**Where the ceremony actually bites is P2, not the spine.** The spine's call is already free — a bare `Text`
return, nothing to marshal. The three sharp edges below (retain-on-store, real RC headers, copy-before-release)
exist precisely *because* a Rust host has to hand-construct Coddl's RC-headed values across the seam. An
all-Coddl host would hand the handler a native value directly and none of that hand-marshalling would exist.
That is the concrete payoff of eventually moving the loop into Coddl — and the concrete reason it is deferred:
it needs Coddl to grow a `Binary` type and socket builtins first.

## Responsibilities

**The host (`coddl-web`, Rust) owns:** the TCP listener; HTTP/1.1 parsing; calling `coddl_app_init` once at
startup and `coddl_app_shutdown` once at teardown; per-request marshalling of a `Request` value in and a
`Response` value out; JSON (de)serialization; releasing every payload a handler returns. It calls **one**
handler entry per app — `handle` — as an opaque C function across the `extern "C"` ABI, exactly the
compiler/runtime boundary [workspace.md](workspace.md) defines. It knows no relational algebra, **and no
routing** — see the design note below.

**Coddl provides:** relvars (the models) and handler `oper`s (the views). A handler is an ordinary `oper`;
nothing about it is web-specific except the shape of its parameter and result. The relational query inside a
handler pushes to SQL like any other. **Routing lives here too** — the app's single `handle` inspects the
`Request` and delegates, in Coddl.

### Design note: `coddl::web` is vocabulary, not a framework

Routing is *application logic*, not *transport*, so by the self-hosting fault line it sits **above** the FFI
bottom — in Coddl, not in the host. The host therefore stays at **one `handle` symbol** forever and never
grows a router; the whole "how does a generic host bind app-specific handler symbols" question dissolves. An
app's `handle` is itself the router: it reads `req.method` / `req.path` and delegates to sub-`oper`s. A "web
framework" is then a **userspace Coddl library** of those idioms, not something the host or stdlib imposes.

Correspondingly, the [`coddl::web`](prelude.md) module holds **no opinion on routing** (or middleware). It is
two tiers: (1) the **contract types** `Request` / `Response`, which are mandatory because they are the literal
ABI the host marshals; and (2) **assumption-free primitives**, added only demand-driven. Because
`Request` / `Response` are an ordinary tuple + relation, **core algebra already expresses most of what a
framework wants** (`req.headers where name = "host"`, `resp replace { status: 404 }`), so the module stays
thin by default — and a thin module can't leak policy. Add an oper only when core algebra genuinely can't
express it *and* a real framework has shown the pattern repeats (promote proven idioms **down**; don't
speculate a grab-bag API **up**). Even a "neutral" helper can smuggle policy — a `header { req, name }` oper
must pick absent-header behavior — so prefer the form that hands back the value/relation and lets the caller
decide. Fuller pattern/param routing waits on Coddl `Text` primitives (push to each backend's native string
functions), and relvar-driven dispatch-by-data on first-class `oper` references — both **language** features,
deliberately not host workarounds.

## The boundary: the request/response ABI

A request crosses the C ABI as ordinary Coddl values. The calling convention that already exists
([codegen.md](codegen.md)) does most of the work; the one genuinely missing piece is on the return side.

### Argument (host → handler)

A handler receives the **cooked `Request`** (`crates/coddl-stdlib/modules/coddl/web.cd`, opt-in via
`use module coddl::web;`): the host decodes the raw request-target into queryable relations, so routing and
path-param extraction are plain relational algebra with **no** dependency on Text primitives (ROADMAP D1 —
decode *policy* sits in the host provisionally, reversible to userspace once those primitives land):

```
Request : Tuple { method: Text, path: PathSegments, query: OrderedNameValues,
                  headers: OrderedNameValues, raw_path: RawRequestPath,
                  raw_query: RawRequestQuery, body: Text }
```

`path` is `PathSegments = Relation { ordinality: Integer, segment: Text }` — the URL path split on `/` and
percent-decoded, one tuple per segment (a **relation, not a `Sequence`**: `ordinality` is a mandatory
attribute, so `/a/a` stays two tuples; RM Pro 1). `query` is `OrderedNameValues = Relation { name, value,
ordinality: Integer }` — the `&`-separated pairs, `+`→space and `%XX` decoded; `headers` is the same relation
with names lowercased (ordinality carries wire order). `raw_path` / `raw_query` keep the original
still-encoded target parts (single-possrep scalars wrapping the raw octets, physically `Text`) as an escape
hatch — and make the record wide enough to stay boxed. The raw `RawRequest` type is retained in `web.cd` for
a future raw-bytes need but is no longer what the host builds.

At **88 B**, `Request` is **≥ the 64 B boxing threshold**, so it crosses the ABI **boxed**: one pointer to a
name-sorted record `body@0 (Text), headers@16 (Relation ptr), method@24 (Text), path@40 (Relation ptr),
query@48 (Relation ptr), raw_path@56 (Text), raw_query@72 (Text)`. So `oper handle { req: Request }` lowers
to `define ptr @handle(ptr %req)` and reads a field with an `AttrLoad` at its offset — routing on the path is
`req.path where ordinality = 0 and segment = "wiki"`, a plain relational query on the relation cell at
offset 40. (These offsets are pinned to the compiler's `record_layout` by a test in `coddl-procir` and to the
host's hand-written descriptor by a round-trip test in `coddl-web`, guarding the silent descriptor-mismatch.)

The host builds the boxed `Request` record by hand (`build_request` in `coddl-web/src/main.rs`): `rc_text`
cells for `method`/`body`/`raw_path`/`raw_query`, a `build_path_segments` relation for `path`, and
`build_headers` relations for `query` and `headers` (against the `PathSegments` / `OrderedNameValues`
descriptors, one record per element with its ordinality) — all *moved in* (rc = 1). The request-target is
split at the first `?`; the path is then split on `/` and each segment percent-decoded **after** the split
(so `/a%2Fb` is one segment `a/b`), and the query pairs are `+`→space + `%XX` decoded (pure helpers,
unit-tested for the edge cases). Because the record owns every cell, **one** `coddl_rc_release` of the record
cascades through the drop walker and frees them all — the three nested relations self-describe via their own
RC-header descriptors, so the parent's Relation cells carry a null sub-descriptor.

### Return (handler → host)

**A handler returns a `RawResponse` `Tuple` by value.** Whole-`Tuple`-by-value return exists via
**box-on-return**: a `Tuple` in return position is *always* returned as one `ptr` to a length-1
`CoddlKind::Relation` record (`box_return_value_if_needed` in [procir.md](procir.md); the boxing is
data- and cardinality-preserving, so `TupleBox`/`TupleUnbox` round-trips are transparent). So

```
oper handle { req: Request } -> RawResponse
```

lowers to `define ptr @handle(ptr %req)` returning one pointer to a 32-byte record with the name-sorted
layout `body@0 (ptr@0, len@8), headers@16 (Relation ptr), status@24 (Integer i64)`. The host
reads the record's cells directly through that layout — `status`, the `body` `(ptr, len)`, and the `headers`
relation pointer (walked into `(name, value)` pairs and emitted as HTTP header lines) — exactly as it reads
any returned record. This reuses `coddl_query`-style `*mut u8` returns and pulls in **zero** plan/relvar
machinery: reading a returned record touches only the RC header, and the record's baked-in descriptor drives
the drop walker on release (freeing the inner `body` `Text` *and* the `headers` relation and its cells).
Transport framing headers (`Content-Length`, `Connection`) are the host's — a handler-supplied copy is
dropped so it can't fight the host's connection management.

> **Superseded design.** Earlier drafts specced two lower-capability paths — a `-> Text` spine (body only,
> status/headers hardcoded host-side) and `Response` as a *single-tuple `Relation`* — because
> whole-`Tuple`-by-value return did not yet exist. Both are obsolete: the handler returns a `Response`
> `Tuple` directly (one boxed-record pointer), not a relation of one tuple. There is no `-> Text` body plus a
> bespoke `status_out` param, and no `sret`.

### Three sharp edges (where "just reuse the marshalling" is subtly wrong)

1. **Copy the body out before releasing the record.** The response body is a `Text` cell *inside* the boxed
   `Response` record; the drop walker (`drop_relation_payload`, [memory.md](memory.md)) frees that inner cell
   when the host releases the outer record. The host must copy the bytes to its own buffer first, or the body
   pointer dangles. (`read_response` in `coddl-web` copies before the single `coddl_rc_release`.)
2. **Release every returned payload, exactly once per request.** The host owns the handler's result and must
   `coddl_rc_release` it after serializing — otherwise it leaks one relation/`Text` per request. Release
   no-ops on immortal string literals (`IMMORTAL_RC`), so a uniform release is always correct.
3. **A stored `Text` param needs a real RC header.** Handler parameters are *borrowed*, so a handler that only
   reads its request is safe with a raw `(ptr, len)`. But if a handler *stores* a request `Text` into a
   relation, retain-on-store reads a `CoddlRcHeader` 32 bytes ahead of the pointer — UB if the host passed
   bytes with no header. So `coddl-web`'s `rc_text` builds every request `method`/`path`/`body` through
   `coddl_rc_alloc` (a real header), not a raw `(ptr, len)`, replicating the RC-headed-cell discipline that
   `marshal_rows` uses — even though the current handler only reads.

### Layout single source of truth

The host is a **new consumer** of the record layout defined once in `crates/coddl-procir/src/layout.rs`
(`cell_width` / `cell_kind` / `record_layout`) and mirrored by the runtime's `#[repr(C)]` types and both
codegen backends. `coddl-web` now hand-writes two `CoddlHeadingDesc`s against this layout — the
`{ name, value }` `headers` heading and (for the built-in default handler) the `{ body, headers, status }`
`Response` record — mirroring the `coddl::env` descriptor precedent in `crates/coddl-runtime/src/env.rs`.
There is no automated drift check ([risks.md](risks.md), "FFI struct-layout single source of truth"): these
hand-written descriptors silently rot if the layout description ever diverges and doesn't become generated.

## Lifecycle: `coddl_app_init` / `coddl_app_shutdown` — **DONE (P1b)**

A web handler is a **`library`** (`library web_hello;`) — its file-kind header declares it a
stable-C-ABI artifact a foreign host links, not "a `program` that happens to have no `main`."
The lowerer keys lifecycle emission off that declared kind (`header_is_program`), so the two
paths below are chosen by the header, not inferred from the absence of an `oper main`.

For a `program` the runtime lifecycle rides `main`: the lowerer wraps `oper main {}` with
`coddl_runtime_init` / `coddl_runtime_shutdown` and splices in `RegisterDatabase` / `RegisterPlan` (one per
pushed SQL plan) and per-relvar `RelvarSlotInit` / `RelvarSlotRelease`. A web host owns `main`, so that
sequence can't ride one — a `library` / `module` takes the mainless branch in `finalize_main_prologue`
(`crates/coddl-procir/src/lower.rs`) instead.

The registration sequence is compiler-synthesized from the plan and **independent of any user `main` body**,
so a **mainless module now synthesizes two fresh exported functions** — `coddl_app_init` (a
`coddl_runtime_init` call + the same `RegisterDatabase` / `RegisterPlan` / `RelvarSlotInit` /
`PrivateRelvarSlotInit` prologue) and `coddl_app_shutdown` (the `RelvarSlotRelease`s + `coddl_runtime_shutdown`)
— carrying the identical sequence, rather than relocating anything out of a `main` a web app never had. Both
codegen backends emit them unchanged (they already lower those instructions and every `Function`).

**Both are always emitted for a mainless module** — even a DB-less one (then they're just
`runtime_init`/`shutdown`) — so a *generic* host can call them unconditionally and the symbols always resolve
(the earlier "a DB-less spine may skip them" note is superseded — always-emit no-op stubs is simpler than a
weak-symbol dance). `coddl-web` declares them under `#[cfg(coddl_app_obj)]` and calls `coddl_app_init` once
before the accept loop, with an RAII guard that runs `coddl_app_shutdown` on exit (best-effort — the loop
runs until the process is killed; a skipped shutdown just leaks the pool at exit, benign). The contract is
process-lifetime: the runtime's registries are global `Mutex<HashMap>`s built for init-once / use-many /
release-once, so calling `app_init` per request would double-register plans (`coddl_register_plan` aborts on
a duplicate `plan_id`).

The mechanism is proven by the hermetic e2e `app_init_shutdown_drive_a_mainless_relvar_handler`: a mainless
module with a **private relvar** that an oper writes then reads, a C host that calls
`coddl_app_init` → the oper → `coddl_app_shutdown`, run under the leak gate. (The **public-relvar + SQL +
database** path in `app_init` is carried by the same synthesized code but is exercised by P4 — a handler
whose body is a relvar query pushed to SQL, which needs a seeded database.)

## The `coddl-web` crate

A new crate at the host boundary, a sibling of [`coddl-driver`](driver.md): it owns transport and knows no
relational algebra — and no routing — exactly as the driver calls the frontend without knowing SQL. Per
request: a single-threaded HTTP/1.1 `TcpListener` parses the request, calls the app's single `handle`,
reads the response, serializes, writes, releases; loop. (`coddl_app_init`/`_shutdown` enter once a handler
touches a relvar — P1b.)

**Single-threaded first is forced, not chosen.** The runtime assumes one linear program run: global
Mutex-guarded registries (connections, databases, plans, relvar slots), a global atomic transaction-depth
counter, and one connection per database path. A concurrent per-request model collides with all three; it is
deferred (see below), and the spine proves the boundary without touching it.

**Routes are data (a future note) — in *userspace* Coddl, not the host.** A route table is a relvar —
`Routes { name: Text, method: Text, pattern: Text, handler: Text }` — and the two directions are two queries
over it: *forward* dispatch (`method` + `path` → `handler`) is a restriction, and *reverse* URL resolution
(`name` + args → URL) is a lookup plus substitution. Django keeps a resolver and a separate reverse map;
relationally they are one relvar queried two ways. This lives **above** the FFI line — a userspace Coddl web
framework owns it; the host never routes (it stays at one `handle`). There is precedent for a stdlib module
exporting a *relvar* rather than only types: `coddl::env`'s `Environment` is a `builtin relvar` (read *and*
written through ordinary relational DML — see [prelude.md](prelude.md)), so a framework's `Routes` relvar is
the same shape. Two language capabilities gate the ergonomic form: `Text` primitives for pattern/param
matching (pushable to backends' native string functions), and first-class `oper` references to *call* the
handler a matched row names. `coddl::web` itself stays neutral — it ships neither `Routes` nor a router.

<!-- DESIGN-AHEAD-XREF: provisional cross-reference from this authoritative doc into a wiki-example
artifact. Remove it (or fold the design into this doc) when F7 lands, or when the wiki framework
extracts to its own repo (ROADMAP D4) and this path breaks. Grep `DESIGN-AHEAD-XREF` for every such marker. -->
This sketch has since been fully scoped — and refined past the single-`pattern`-column shape shown above — in
[`examples/wiki/routing-design.md`](../examples/wiki/routing-design.md): the route table is vertically
decomposed (`Routes` / `RouteLiterals` / `RouteParams` + the `OperParams` catalog — no nulls, RM Pro 4),
`handler` is a module **path** (not a pointer, RM Pro 7), typed params ride a `to_url_regexp` protocol, forward
matching is simulated relational division, and ambiguity is a forward-key / specificity constraint (ties
rejected). See ROADMAP items **F7 / L6 / L7**. <!-- /DESIGN-AHEAD-XREF -->

## Deployment

The deployable unit is a **single self-contained native binary**: the `coddl-web` host loop, the app's
compiled handlers and relvars, and `libcoddl_runtime`, statically linked into one executable that listens on
a port and talks to SQLite/Postgres. There is no "Coddl server" you install and drop scripts into — the app
*is* the server.

Integrating with Apache/nginx is therefore mostly *not* a code coupling. The default is a **reverse proxy**:
the Coddl binary serves plain HTTP on a loopback port, and Apache/nginx sits in front for TLS termination,
static files, and load balancing, forwarding requests over localhost. "Plugging into Apache" is a few lines
of `ProxyPass` / `ProxyPassReverse`, not a module. This is how Django (gunicorn), Rails (puma), Go, and Node
deploy, and it keeps the web server entirely out of our ABI — nothing links into anything, which is the
FFI-decoupling discipline of [principles.md](principles.md) applied one layer out.

```
                 ┌──────────────────────────────────────────┐
  Internet ────► │ Apache / nginx  (reverse proxy)          │
                 │   TLS termination · static files         │
                 │   ProxyPass / → http://localhost:8000/   │
                 └──────────────────┬───────────────────────┘
                                    │ plain HTTP, localhost
                                    ▼
                 ┌──────────────────────────────────────────┐
                 │  the Coddl app — ONE native binary        │
                 │  ┌────────────────────────────────────┐  │
                 │  │ coddl-web host loop (Rust)         │  │  ← the "uvicorn"
                 │  │   socket · HTTP parse · routing    │  │
                 │  └───────────────┬────────────────────┘  │
                 │                  │ C ABI (handler calls)  │
                 │  ┌───────────────▼────────────────────┐  │
                 │  │ handlers + relvars (Coddl)         │  │  ← the app
                 │  └───────────────┬────────────────────┘  │
                 │  ┌───────────────▼────────────────────┐  │
                 │  │ libcoddl_runtime (Rust)            │  │
                 │  └───────────────┬────────────────────┘  │
                 └──────────────────┼───────────────────────┘
                                    ▼
                            SQLite / Postgres
```

The full space of integration models, cheapest-coupling last:

| Model | What it is | Coupling / cost |
|---|---|---|
| **Reverse proxy** *(default)* | The Coddl binary is its own HTTP server; Apache/nginx proxies to `localhost:8000`. | Least coupling — a config snippet; Apache never links our code. What the `coddl-web` listener targets. |
| **CGI** *(bootstrap)* | Apache spawns the binary per request; request in via environment + stdin, response out via stdout. | Near-free *today* — an ordinary [`coddl compile`](driver.md) executable with a `main`; no library emission, no new ABI. Slow under load (a process spawn per request). |
| **FastCGI / SCGI** | A long-lived process Apache talks to over a socket in the FastCGI protocol. | Medium — needs a FastCGI codec in the host. Largely superseded by the proxy model. |
| **`mod_coddl`** | The app compiled as a `cdylib` the web server `dlopen`s, calling handlers via an Apache module ABI. | Tightest coupling (mod_php-style); most work, most fragile. The eventual home of the `cdylib` path [workspace.md](workspace.md) anticipates — last to build, if ever. |

Two of these need **nothing new** from the compiler:

- **CGI is the near-free bootstrap.** A CGI program is just an executable that reads the request from the
  environment + stdin and writes the response to stdout — which is exactly a `coddl compile` binary with a
  `main`, no mainless emission (P1a) and no new return ABI. It is slow, but it is a real "running behind
  Apache" milestone reachable *before* the host loop exists — useful for an early demo.
- **Reverse proxy is what the rest of this doc builds toward.** The `coddl-web` listener is the app server;
  Apache is optional infrastructure in front of it, and the two never share an ABI.

`mod_coddl` ties us to Apache's module internals for no gain over the proxy model, so it is the last thing to
build — the `cdylib` path is deferred with the rest of the plugin-loading story.

## Sequence

The ordering is dependency-driven; the one non-obvious point is that lifecycle synthesis (P1b) is a
dependency of the database payoff (P4), **not** of the request/response plumbing (P2/P3).

1. **P0 — resolve the RC dealloc contract.** The per-request model leans on a returned relation payload being
   freed correctly. On main this is still open risk #12 ([risks.md](risks.md)): `coddl_rc_release` recomputes
   the dealloc `Layout` from the header's `length`, which `coddl_relation_seal` *shrinks* on dedup — so an
   allocate-N, seal-to-M, release path is size-mismatched. `coddl_query` deliberately does not seal
   (`length == capacity`), so the *query* path is safe today; but resolve the general case (store the
   allocated count in the header and free against it) before per-request relation churn, and keep the
   host-releases-every-returned-payload discipline.

2. **P1a — mainless library emission. DONE.** A `.cd` with only handler `oper`s and no `main` already lowers
   (the prologue synthesis early-returns when no `main` exists) and both backends emit each `oper` as its own
   C symbol (no name mangling — the surface name *is* the linkage name). No new driver work was needed: the
   emission mode that stops at the object file is [`coddl emit-obj`](driver.md) (Cranelift object), which
   already existed. A host build links its output against `libcoddl_runtime.a` instead of
   [`coddl compile`](driver.md) linking an executable. (An LLVM-quality `.o` via `llc`/`clang -c` remains
   net-new driver work, deferred; the spine uses the Cranelift object.)

3. **Spine. DONE.** The `coddl-web` crate: a single-threaded `TcpListener` that ignores the request, calls
   the handler, writes a fixed `200 OK`, copies the body out, releases the payload, loops. It links a handler
   two ways — a built-in `hello\n` default (so `cargo run -p coddl-web` serves out of the box) or, with
   `CODDL_APP_OBJ` set, a separately-compiled `oper handle {} -> Text` object. This proves the entire FFI
   boundary — mainless codegen, handler symbol linkage, the `Text`-return ABI, staticlib linkage into a
   foreign host, immortal-literal RC — with no relvar in sight. The de-risking spike (see Verification) is
   automated as the driver e2e test `web_spine_mainless_handler_links_into_c_host`, which links a mainless
   object into a C host exactly as the spike prescribes.

4. **P2 — richer request/response. DONE.** The handler takes a `Request` `Tuple` parameter (flattened to 7
   ABI args) and returns a `Response` `Tuple` by value (box-on-return; the host reads status + body from the
   record). The host builds RC-headed request `method`/`path`/`body` `Text`s + a (then-empty) `headers`
   relation, calls across the ABI, copies the body out, and releases every payload exactly once — all three
   sharp-edge mitigations landed. The default handler hand-builds a `Response` record; a compiled
   `oper handle { req: Request } -> Response` (`examples/web-hello/handle.cd`) links in via `CODDL_APP_OBJ`.
   (`headers` crossed **empty** at P2; P3a fills them — see below.)

5. **P3a — real HTTP parsing + populated headers. DONE.** The host parses real HTTP requests (headers +
   `Content-Length` body framing, accumulated across `read`s) and carries headers **both ways**: request
   headers become a populated `{ name, value }` relation on the `Request` (mirroring `coddl_env_snapshot`),
   and the `Response`'s headers relation is walked back into the reply (framing headers stay host-owned).
   Pure host-side Rust — no relvars, no lifecycle, no routing.

6. **RawRequest reshape — possrep-scalar contract, boxed handler. DONE.** The contract became the raw,
   RFC-faithful `RawRequest`/`RawResponse` — `path`/`query` as single-possrep scalar types, `ordinality`
   headers. At 72 B, `RawRequest` crosses the boxing threshold, so the handler takes a **boxed parameter**
   (`define ptr @handle(ptr %req)` — the first ≥64 B param; both backends already had the boxed-param ABI).
   The host builds the boxed request record, splits the target at `?`, and `examples/web-hello/handle.cd`
   echoes `req.path.value` end-to-end over `curl`. Needed two small compiler fixes: a module alias's fields
   resolve to their scalar type (not `Unknown`), and a `Scalar` heading attribute erases to its
   1-field-tuple form so `req.path.value` lowers (`AttrLoad` → `TupleField`).

7. **Routing — a userspace Coddl framework (not host work).** The host stays at **one `handle` symbol**; an
   app's `handle` inspects the cooked `Request` and delegates, in Coddl (see the design note under Responsibilities).
   Exact-path dispatch works today; ergonomic pattern/param routes wait on Coddl `Text` primitives, and
   relvar-driven dispatch-by-data on first-class `oper` references — both language features, not host
   workarounds.

8. **P1b — lifecycle synthesis. DONE.** A mainless module synthesizes `coddl_app_init` / `coddl_app_shutdown`
   (the `main` prologue/epilogue — runtime init + `RegisterDatabase`/`RegisterPlan`/relvar-slot init, and the
   releases + runtime shutdown), always emitted so the host calls them unconditionally; `coddl-web` calls
   them once around the accept loop. Proven with a private-relvar handler + C host under the leak gate; the
   public-relvar SQL path rides the same code, exercised by P4.

9. **P4 — payoff. DONE.** A handler whose body is a relational query against a SQLite-backed `public relvar`,
   pushed to SQL, serialized into the response body — the "Django view + ORM query," null-free and
   backend-pushed. `examples/web-users/` is the worked example (companions + `seed-db.sh`); `curl`ing it
   returns the active users, one line per row.

   The handler **builds its own response body** — it queries, `load`s the result, and loops to concatenate a
   `Text`. There is no relation-serialization primitive and no host-side JSON: serialization is ordinary
   handler code (the `coddl::web` vocabulary stays neutral). A `dump_relation { self: Relation H } -> Text`
   builtin (the value-returning twin of `write_relation`) was considered and deferred — the handler doesn't
   need it.

   ```
   oper handle { _req: Request } -> RawResponse [
       let active = transaction [ Users where active project { name, email } ];
       var rows;  load rows from active order [ asc name ];
       var body := "";
       for r in rows do [ body := body || r.name || " <" || r.email || ">\n"; ];
       { status: 200,
         headers: Relation { { name: "Content-Type", value: "text/plain", ordinality: 0 } },
         body }
   ];
   ```

   Two capabilities landed under P4 to make the handler expressible: **loop-carried owned `Text`** (the
   `body := body || …` accumulator across a loop — the deferred T0076 case, now RC-correct on both backends),
   and **bare-Boolean predicate pushdown** (`where active` ≡ `where active = true`, pushed as `WHERE
   "active" = ?`; the formatter canonicalizes to the bare form, so both must push). Proven by the hermetic
   e2e `app_init_drives_a_mainless_public_relvar_sql_query` (query pushed to SQL through the synthesized
   `coddl_app_init`, under the leak gate) and the manual `coddl-web` + `curl` flow above.

10. **H1 — cooked `Request` transform. DONE.** The host now decodes the request-target and hands the handler
    the cooked `Request` (above) instead of the raw `RawRequest`: `path` a percent-decoded
    `PathSegments = { ordinality, segment }` relation, `query`/`headers` decoded `OrderedNameValues`, with
    `raw_path`/`raw_query` kept alongside. So routing and path-param extraction are plain relational queries
    (`req.path where ordinality = 0 and segment = "wiki"`) with **no** dependency on Text primitives (ROADMAP
    D1). Carrying the raw parts keeps the record at 88 B — boxed, so the ABI stays `handle(ptr) -> ptr`
    unchanged; only the host's descriptor + a decoding `build_request` changed. The offsets are pinned both
    ways (a `record_layout` assertion in `coddl-procir`, a `build_request` round-trip in `coddl-web`);
    percent/`+`/split-before-decode edge cases are unit-tested; and a probe handler reading `req.path` over the
    real host echoes decoded segments (`/wiki/Home%20Page` → `Home Page`, `/a%2Fb/x` → one segment `a/b`).
    Unblocks userspace routing (ROADMAP F1/F2).

## What's deferred

- **Concurrency / re-entrancy** — the global registries, the atomic transaction-depth counter, and the
  one-connection-per-path pool all assume a single linear run; a threaded or async per-request model is a
  runtime-layer effort of its own.
- **Real JSON** — host-side, and *derivable from the handler heading* (the heading is the schema, the same
  thing Pydantic does by hand). Not in the relational middle.
- **Binary / byte request bodies** — need the `Binary` type, which has no literal syntax yet (a grammar
  decision); bodies stay `Text` until then.
- **Handler overloading** — needs name mangling; the surface name is the linkage name today.
- **`cdylib` hot-reload / plugin loading** — the `staticlib`-into-foreign-host cut ships first;
  [workspace.md](workspace.md)'s `cdylib` path is the later evolution.
- **Chunked transfer + request pipelining** — the host frames the body by `Content-Length` only; a chunked
  body is treated as empty, and bytes past `Content-Length` are dropped (`Connection: close`, no pipelining).
- **Routing as a userspace Coddl framework** — the host stays at one `handle`; routing (incl. a `Routes`
  relvar queried two ways for forward dispatch + reverse URL resolution) is userspace, gated on Coddl `Text`
  primitives and first-class `oper` references (see the Responsibilities design note).

## Verification

The spine doubles as the cheapest experiment that de-risks the whole plan (≈ half a day). The single riskiest
assumption is that a *foreign* host owning `main` can link a mainless Coddl object against the runtime and
call a handler across the C ABI — the runtime was written assuming the compiled program owns startup and
shutdown, and every existing test drives a program *with* a `main`. Kill that unknown first:

```
program p;
oper handle {} -> Text [ "hello\n" ];
```

`coddl emit-obj handle.cd -o handle.o`, then `cc handle.o libcoddl_runtime.a small_host.c -o t`, where
`small_host.c` declares `extern char* handle(size_t*);`, calls it, writes the bytes, and calls
`coddl_rc_release`. If it links and prints `hello`, it simultaneously proves P1a, the P3 stub, staticlib
linkage into a foreign host, the `Text`-return out-param ABI, and immortal-literal RC read/release — the
entire boundary the rest of the milestone stands on.
