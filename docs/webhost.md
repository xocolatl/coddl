# Web host — compiled Coddl behind an HTTP server

How a Coddl program serves HTTP requests, Django/FastAPI-shaped: a Rust **host** owns the socket loop,
HTTP parsing, routing, and JSON; Coddl provides the **models** (relvars) and the **handlers** (`oper`s),
compiled as a host-callable library. The host builds a request value, calls a handler, and serializes the
response it gets back.

> **Status.** *Request/response built.* The vocabulary this doc marshals — `Request` / `Response` — is real:
> it lives in the opt-in [`coddl::web`](prelude.md) standard-library module, brought into scope with
> `use module coddl::web;`. **P1a, the Spine, and P2 (below) have landed:** `coddl emit-obj` produces a
> mainless object, and the `coddl-web` crate is a single-threaded `TcpListener` that parses each request,
> marshals a `Request` `Tuple` in, calls a handler across the C ABI, reads a `Response` `Tuple` back
> (box-on-return), and writes the reply. What remains is routing (P3), lifecycle synthesis (P1b), and the
> relvar-backed payoff (P4). It extends the first milestone: everything it relies on — the frontend, RelIR →
> SQL, ProcIR → native codegen, the `staticlib` runtime, user `oper`s with the C-ABI return convention — is
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
`void*` RC payload pointer. The `Request` above flattens to 7 args — `body (ptr, len)`, `headers (ptr)`,
`method (ptr, len)`, `path (ptr, len)` — which `coddl-web` builds and pushes per request.

The host builds the `headers` relation value directly (see [memory.md](memory.md), "One-reference-per-cell")
via `coddl_rc_alloc` against a hand-written `{ name, value }` descriptor — an **empty** relation for now,
so the plumbing is exercised without host-side header parsing (populated headers are the follow-up).

### Return (handler → host)

**A handler returns a `Response` `Tuple` by value.** Whole-`Tuple`-by-value return now exists via
**box-on-return**: a `Tuple` in return position is *always* returned as one `ptr` to a length-1
`CoddlKind::Relation` record (`box_return_value_if_needed` in [procir.md](procir.md); the boxing is
data- and cardinality-preserving, so `TupleBox`/`TupleUnbox` round-trips are transparent). So

```
oper handle { req: Request } -> Response
```

lowers to `define ptr @handle(<flattened Request args>)` returning one pointer to a 32-byte record with the
name-sorted layout `body@0 (ptr@0, len@8), headers@16 (Relation ptr), status@24 (Integer i64)`. The host
reads the record's cells directly through that layout — `status` and the `body` `(ptr, len)` — exactly as it
reads any returned record. This reuses `coddl_query`-style `*mut u8` returns and pulls in **zero**
plan/relvar machinery: reading a returned record touches only the RC header, and the record's baked-in
descriptor drives the drop walker on release (freeing the inner `body` `Text` *and* the `headers` relation).

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
   record). The host builds RC-headed request `method`/`path`/`body` `Text`s + an empty `headers` relation,
   calls across the ABI, copies the body out, and releases every payload exactly once — all three sharp-edge
   mitigations landed. The default handler hand-builds a `Response` record; a compiled
   `oper handle { req: Request } -> Response` (`examples/web-hello/handle.cd`) links in via `CODDL_APP_OBJ`.
   `headers` on both sides is an **empty** relation for now — populated request/response headers are the
   follow-up.

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
- **Populated `headers`** — request and response `headers` cross the ABI as an *empty* `{ name, value }`
  relation today. Filling them host-side (parsing request headers into records; emitting response header
  records) is the immediate follow-up; the relation-valued-attribute plumbing it rides on already works.
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
