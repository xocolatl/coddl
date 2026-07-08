# Web host — compiled Coddl behind an HTTP server

How a Coddl program serves HTTP requests, Django/FastAPI-shaped: a Rust **host** owns the socket loop,
HTTP parsing, routing, and JSON; Coddl provides the **models** (relvars) and the **handlers** (`oper`s),
compiled as a host-callable library. The host builds a request value, calls a handler, and serializes the
response it gets back.

> **Status.** *Partly built.* The vocabulary this doc marshals — `Request` / `Response` — is now real: it
> lives in the opt-in [`coddl::web`](prelude.md) standard-library module, brought into scope with
> `use module coddl::web;`. The *host* (`coddl-web`, the socket loop) is still planned; this doc remains the
> work plan for it. It extends the first milestone: everything it relies on — the frontend, RelIR → SQL,
> ProcIR → native codegen, the `staticlib` runtime, user `oper`s with the C-ABI return convention — is
> assumed already end-to-end per [milestone.md](milestone.md).

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
| `@app.get("/users")` handler | `oper handle { req: Request } -> Response` | relational middle |
| uvicorn accept loop + HTTP parse + routing + JSON | the **Rust host** (`coddl-web`) | FFI bottom |

The transport half being absent from Coddl is *correct*, not a gap — it isn't the app developer's job in
these frameworks either. It's **FFI-bottom "will stay Rust" work** by the self-hosting fault line in
[principles.md](principles.md): sockets, byte parsing, and JSON need the pointers and raw memory the surface
language deliberately forbids, so they stay in the host and never depend on the relational middle. The
ORM-equivalent half, by contrast, is a thing Coddl does *better* than the frameworks: a relvar query is the
model and the query at once, pushed to the backend, with no null (RM Pro 4) and no impedance mismatch.

The project already anticipates this shape: the runtime is a `staticlib` today, "`cdylib` later if plugin
loading lands" ([runtime.md](runtime.md), [workspace.md](workspace.md)). A web host is that same
embed-compiled-Coddl-as-a-library move, driven by a concrete application.

## Responsibilities

**The host (`coddl-web`, Rust) owns:** the TCP listener; HTTP/1.1 parsing; the route table
(method + path → handler symbol); calling `coddl_app_init` once at startup and `coddl_app_shutdown` once at
teardown; per-request marshalling of a `Request` value in and a `Response` value out; JSON (de)serialization;
releasing every payload a handler returns. It knows no relational algebra — it calls handlers as opaque C
functions across the `extern "C"` ABI, exactly the compiler/runtime boundary [workspace.md](workspace.md)
defines.

**Coddl provides:** relvars (the models) and handler `oper`s (the views). A handler is an ordinary `oper`;
nothing about it is web-specific except the shape of its parameter and result. The relational query inside a
handler pushes to SQL like any other.

## The boundary: the request/response ABI

A request crosses the C ABI as ordinary Coddl values. The calling convention that already exists
([codegen.md](codegen.md)) does most of the work; the one genuinely missing piece is on the return side.

### Argument (host → handler)

Model a request as a Coddl `Tuple`:

```
Request : Tuple { method: Text, path: Text, headers: Relation { name: Text, value: Text }, body: Text }
```

The canonical `Request` / `Response` declarations live in the [`coddl::web`](prelude.md) module
(`crates/coddl-stdlib/modules/coddl/web.cd`, opt-in via `use module coddl::web;`); this sketch mirrors them.

A `Tuple` parameter is **flattened per-attribute** into leaf ABI slots in canonical (name-sorted) heading
order — `Text` as a `(ptr, i64)` pair, scalars by value — so the host passes a request by pushing its fields
as C arguments; no new convention is required. A relation-valued attribute (`headers`) passes as an opaque
`void*` RC payload pointer.

*First cut:* model `headers` and `body` as `Text` and skip the relation-valued attribute — building a
`Relation` value host-side (see [memory.md](memory.md), "One-reference-per-cell") is deferred until the
spine works.

### Return (handler → host)

There is **no whole-`Tuple`-by-value return** today — `Tuple` is required to be flattened at ABI boundaries,
and a bare `Tuple` in scalar return position is `unreachable!` in codegen (it would need `sret` / return-pair
machinery). So the return uses one of two existing paths, in order of increasing capability:

1. **Spine — `oper handle { … } -> Text`.** Return the response body directly and reuse the fully-built
   fat-pointer return convention: the function gains a trailing `*mut usize` length-out parameter and returns
   the payload pointer (`define ptr @handle(…, ptr %.ret_len_out)`; see [codegen.md](codegen.md)). Status and
   headers are hardcoded host-side (200, `text/plain`). This is enough for the entire first milestone.
2. **`Response` as a single-tuple `Relation`.** When status/headers must ride along, declare the handler to
   return a `Relation` of exactly one tuple and reuse the relation return path (an `oper` with a non-unit
   result already returns its payload pointer, kept alive past scope by escape retention). The host reads
   record 0 through the heading descriptor. This reuses `coddl_query`-style `*mut u8` returns and pulls in
   **zero** plan/relvar machinery — reading a returned relation touches only the RC header.

Whole-`Tuple`-by-value (`sret`) and a `-> Text` body plus a bespoke `status_out` param are both possible but
rejected for now: the first needs new return-ABI codegen; the second needs bespoke out-param synthesis and is
strictly less principled than the relation return once headers arrive.

### Three sharp edges (where "just reuse the marshalling" is subtly wrong)

1. **Copy the body out before releasing the relation.** With the relation return, the response body is a
   `Text` cell *inside* the record; the drop walker (`drop_relation_payload`, [memory.md](memory.md)) frees
   that inner cell when the host releases the outer relation. The host must copy the bytes to its own buffer
   first, or the body pointer dangles.
2. **Release every returned payload, exactly once per request.** The host owns the handler's result and must
   `coddl_rc_release` it after serializing — otherwise it leaks one relation/`Text` per request. Release
   no-ops on immortal string literals (`IMMORTAL_RC`), so a uniform release is always correct.
3. **A stored `Text` param needs a real RC header.** Handler parameters are *borrowed*, so a handler that only
   reads its request is safe with a raw `(ptr, len)`. But if a handler *stores* a request `Text` into a
   relation, retain-on-store reads a `CoddlRcHeader` 32 bytes ahead of the pointer — UB if the host passed
   bytes with no header. A host that builds request values a handler may store must replicate the
   RC-headed-cell discipline that `marshal_rows` uses. Harmless for the read-only spine; latent for P2.

### Layout single source of truth

The host is a **new consumer** of the record layout defined once in `crates/coddl-procir/src/layout.rs`
(`cell_width` / `cell_kind` / `record_layout`) and mirrored by the runtime's `#[repr(C)]` types and both
codegen backends. There is no automated drift check ([risks.md](risks.md), "FFI struct-layout single source
of truth") — a host mirroring these `#[repr(C)]` types by hand is one more place that silently rots if the
layout description doesn't become generated. Note it there.

## Lifecycle: `coddl_app_init` / `coddl_app_shutdown`

Today the runtime lifecycle is bolted onto `main`: the lowerer wraps a user `oper main {}` with
`coddl_runtime_init` / `coddl_runtime_shutdown` and splices in `RegisterDatabase` / `RegisterPlan` (one per
pushed SQL plan) and per-relvar `RelvarSlotInit` / `RelvarSlotRelease`. A host owns `main`, so that lifecycle
must move.

The registration instruction sequence is compiler-synthesized from the plan and is **independent of any user
`main` body**. Library mode therefore synthesizes two fresh functions — `coddl_app_init` and
`coddl_app_shutdown` — carrying that identical instruction sequence, rather than trying to relocate anything
*out of* a `main` that a web app never had. This is cleaner than today's approach of hunting for the block
that contains the `coddl_runtime_init` call.

The host calls each **exactly once**, RAII-guarded in `coddl-web`. The contract is process-lifetime: the
runtime's registries are global `Mutex<HashMap>`s built for init-once / use-many / release-once. Calling
`app_init` per request would double-register plans and re-init relvar slots; never calling `app_shutdown`
leaks the connection pool at exit (benign). A DB-less spine has no plans or relvars, so it synthesizes
nothing — `coddl_runtime_init` / `_shutdown` are no-op stubs and the host may skip them entirely.

## The `coddl-web` crate

A new crate at the host boundary, a sibling of [`coddl-driver`](driver.md): it owns transport and knows no
relational algebra, exactly as the driver calls the frontend without knowing SQL. First cut: a
single-threaded HTTP/1.1 `TcpListener`; `coddl_app_init` once; per-request dispatch through a host-side route
table to the handler symbol; read the response, serialize, write, release; loop.

**Single-threaded first is forced, not chosen.** The runtime assumes one linear program run: global
Mutex-guarded registries (connections, databases, plans, relvar slots), a global atomic transaction-depth
counter, and one connection per database path. A concurrent per-request model collides with all three; it is
deferred (see below), and the spine proves the boundary without touching it.

**Routes are data (a future note).** A route table is a relvar —
`Routes { name: Text, method: Text, pattern: Text, handler: Text }` — and the two directions are two queries
over it: *forward* dispatch (`method` + `path` → `handler`) is a restriction, and *reverse* URL resolution
(`name` + args → URL) is a lookup plus substitution. Django keeps a resolver and a separate reverse map;
relationally they are one relvar queried two ways. The pattern-matching and substitution themselves stay
host-side string work (not relational algebra), but the table becomes Coddl data. There is now a concrete
precedent for a stdlib module exporting a *relvar* rather than only types: `coddl::env`'s `Environment` is a
`builtin relvar` (read *and* written through ordinary relational DML — see [prelude.md](prelude.md)). A
`Routes` relvar owned by `coddl::web` is the same shape, queried two ways. Deferred; the first cut is a plain
host-side table.

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

2. **P1a — mainless library emission.** A `.cd` with only handler `oper`s and no `main` already lowers (the
   prologue synthesis early-returns when no `main` exists) and both backends emit each `oper` as its own C
   symbol (no name mangling — the surface name *is* the linkage name). The only new driver work is an
   emission mode that stops at the object file and lets the host's build link it against
   `libcoddl_runtime.a`, instead of [`coddl compile`](driver.md) linking an executable. This is the one true
   universal prerequisite, and it is nearly free.

3. **Spine.** `oper handle {} -> Text [ "hello\n" ]` plus a ~40-line single-threaded `TcpListener` stub that
   ignores the request, calls the handler, writes a fixed `200 OK`, releases the payload, loops. This proves
   the entire FFI boundary — mainless codegen, handler symbol linkage, the `Text`-return ABI, staticlib
   linkage into a foreign host, immortal-literal RC — with no relvar in sight. It is also the de-risking spike
   (see Verification).

4. **P2 — richer request/response.** Add the `Request` `Tuple` parameter (existing flatten) and promote the
   result to a single-tuple `Relation` (`Response`, return option 2). Land the three sharp-edge mitigations.

5. **P3 — host harness proper.** A real host-side routing table and HTTP request parsing. Parallels P2 — it
   only ever needs one handler symbol at a time.

6. **P1b — lifecycle synthesis.** `coddl_app_init` / `coddl_app_shutdown` emitted from the plan. Needed only
   once a handler touches a relvar or a pushed plan — i.e. a dependency of P4, deferred until here.

7. **P4 — payoff.** A handler whose body is a relational query against a relvar, pushed to SQL, returned as
   the response body — the "Django view + ORM query" in a single expression, null-free and backend-pushed:

   ```
   oper handle_users { req: Request } -> Response
     [ Users where active = true project { name, email } ]
   ```

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
- **Host-built relation-valued request attributes** (e.g. `headers` as a relation).
- **Routes as a relvar + reverse URL resolution** — the two-queries-over-one-relvar model above.

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
