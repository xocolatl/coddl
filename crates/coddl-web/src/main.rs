//! `coddl-web` — the web host (see docs/webhost.md), Stage B (`RawRequest`).
//!
//! A single-threaded HTTP/1.1 listener that owns `main`, parses each request
//! into a Coddl `RawRequest` value, calls a handler across the C ABI, reads the
//! `RawResponse` value back, and writes the HTTP reply. It knows no relational
//! algebra and **no routing** — it calls one handler entry (`handle`) per app
//! as an opaque C symbol and lets the app route in Coddl (docs/webhost.md
//! "Design note: coddl::web is vocabulary, not a framework"). Everything
//! web-specific — sockets, HTTP parsing, and the *hand-written marshalling* of
//! Coddl's RC-headed values — is FFI-bottom "stays Rust" work.
//!
//! The handler is `use module coddl::web; oper handle { req: RawRequest } ->
//! RawResponse`. Its ABI (confirmed via `coddl emit-llvm`):
//!   - `RawRequest` (72 B ≥ the 64 B boxing threshold) is a **boxed** tuple
//!     parameter — one pointer to a record, name-sorted layout
//!     `body@0 (Text), headers@16 (Relation), method@24 (Text), path@40 (Text),
//!     query@56 (Text)`. A `Text` cell is a `(ptr, len)` pair; `path`/`query`
//!     are single-possrep scalars (`RawRequestPath`/`RawRequestQuery`) that are
//!     *physically* just their `Text` component.
//!   - `RawResponse` (32 B) is returned **boxed** — one pointer to a record,
//!     `body@0 (Text), headers@16 (Relation), status@24 (i64)`.
//! So the whole call is `handle(req_ptr) -> resp_ptr`. Headers are
//! `OrderedNameValues` — a `{ name, value, ordinality }` relation (40 B records;
//! `ordinality` carries wire order). The surface name is the linkage name (no
//! mangling); the built-in handler below supplies it by default, or `CODDL_APP_OBJ`
//! (see build.rs) links a compiled Coddl handler in its place.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use coddl_runtime::{
    coddl_rc_alloc, coddl_rc_release, CoddlAttrDesc, CoddlAttrKind, CoddlHeadingDesc, CoddlKind,
    CoddlRcHeader, HEADER_SIZE,
};

// ── Hand-written heading descriptors (FFI layout mirror, risks.md #8) ──────
//
// These must match what codegen emits for the same headings. Leaked once so the
// pointers outlive every RC payload that references them.

/// `OrderedNameValues = Relation { name: Text, value: Text, ordinality: Integer }`
/// — the headers relation. Name-sorted, so `name@0 (16 B)`, `ordinality@16
/// (8 B)`, `value@24 (16 B)`; record_size 40. The `ordinality` carries wire
/// order (HTTP header order is defined and lines can repeat).
fn headers_desc() -> *const CoddlHeadingDesc {
    use std::sync::OnceLock;
    static DESC: OnceLock<usize> = OnceLock::new();
    *DESC.get_or_init(|| {
        let attrs: &'static [CoddlAttrDesc; 3] = Box::leak(Box::new([
            CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"ordinality".as_ptr(),
                name_len: 10,
                kind: CoddlAttrKind::Integer as u32,
                offset: 16,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"value".as_ptr(),
                name_len: 5,
                kind: CoddlAttrKind::Text as u32,
                offset: 24,
                sub: std::ptr::null(),
            },
        ]));
        let desc: &'static CoddlHeadingDesc = Box::leak(Box::new(CoddlHeadingDesc {
            attr_count: 3,
            record_size: 40,
            attrs: attrs.as_ptr(),
        }));
        desc as *const CoddlHeadingDesc as usize
    }) as *const CoddlHeadingDesc
}

/// The boxed `RawRequest` record heading — name-sorted `body@0 (Text),
/// headers@16 (Relation), method@24 (Text), path@40 (Text), query@56 (Text)`;
/// record_size 72. `path`/`query` are single-possrep scalars that are physically
/// `Text`, so the drop walker frees them as `Text` — byte-compatible with the
/// compiler's internal 1-field-tuple view of them.
fn raw_request_desc() -> *const CoddlHeadingDesc {
    use std::sync::OnceLock;
    static DESC: OnceLock<usize> = OnceLock::new();
    *DESC.get_or_init(|| {
        let text = |name: &'static [u8], offset: u32| CoddlAttrDesc {
            name: name.as_ptr(),
            name_len: name.len() as u32,
            kind: CoddlAttrKind::Text as u32,
            offset,
            sub: std::ptr::null(),
        };
        let attrs: &'static [CoddlAttrDesc; 5] = Box::leak(Box::new([
            text(b"body", 0),
            CoddlAttrDesc {
                name: b"headers".as_ptr(),
                name_len: 7,
                kind: CoddlAttrKind::Relation as u32,
                offset: 16,
                sub: std::ptr::null(),
            },
            text(b"method", 24),
            text(b"path", 40),
            text(b"query", 56),
        ]));
        let desc: &'static CoddlHeadingDesc = Box::leak(Box::new(CoddlHeadingDesc {
            attr_count: 5,
            record_size: 72,
            attrs: attrs.as_ptr(),
        }));
        desc as *const CoddlHeadingDesc as usize
    }) as *const CoddlHeadingDesc
}

/// The `RawResponse` record heading `{ body: Text @0, headers: Relation @16,
/// status: Integer @24 }` (name-sorted; record_size 32). Used only by the
/// built-in default handler, which builds a record by hand; a compiled handler
/// emits its own equivalent descriptor via codegen.
#[cfg(not(coddl_app_obj))]
fn response_desc() -> *const CoddlHeadingDesc {
    use std::sync::OnceLock;
    static DESC: OnceLock<usize> = OnceLock::new();
    *DESC.get_or_init(|| {
        let attrs: &'static [CoddlAttrDesc; 3] = Box::leak(Box::new([
            CoddlAttrDesc {
                name: b"body".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"headers".as_ptr(),
                name_len: 7,
                kind: CoddlAttrKind::Relation as u32,
                offset: 16,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"status".as_ptr(),
                name_len: 6,
                kind: CoddlAttrKind::Integer as u32,
                offset: 24,
                sub: std::ptr::null(),
            },
        ]));
        let desc: &'static CoddlHeadingDesc = Box::leak(Box::new(CoddlHeadingDesc {
            attr_count: 3,
            record_size: 32,
            attrs: attrs.as_ptr(),
        }));
        desc as *const CoddlHeadingDesc as usize
    }) as *const CoddlHeadingDesc
}

// ── Value builders (host → handler) ────────────────────────────────────────

/// Allocate an owned RC `Text` cell holding `bytes` — a real `CoddlRcHeader`
/// ahead of the payload, so the handler's retain-on-store finds a valid header
/// if it stores the value (webhost.md sharp edge #3).
unsafe fn rc_text(bytes: &[u8]) -> *mut u8 {
    let p = coddl_rc_alloc(
        bytes.len(),
        bytes.len() as u32,
        CoddlKind::Text as u32,
        std::ptr::null(),
    );
    if !bytes.is_empty() {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
    p
}

/// Write a `Text` cell `(ptr, len)` at `cell`, moving in a fresh `rc_text`.
unsafe fn write_text_cell(cell: *mut u8, bytes: &[u8]) {
    (cell as *mut *mut u8).write(rc_text(bytes));
    (cell.add(8) as *mut u64).write(bytes.len() as u64);
}

/// Build an owned `OrderedNameValues` relation from `pairs`, in order — one
/// 40 B record per pair (`name@0`, `ordinality@16` = the index, `value@24`),
/// each `Text` cell a fresh `rc_text` (rc=1, **moved in**). Mirrors
/// `coddl_env_snapshot`; the caller's single release frees every cell via the
/// drop walker. `build_headers(&[])` is the empty relation.
unsafe fn build_headers(pairs: &[(Vec<u8>, Vec<u8>)]) -> *mut u8 {
    let n = pairs.len();
    let rel = coddl_rc_alloc(n * 40, n as u32, CoddlKind::Relation as u32, headers_desc());
    for (i, (name, value)) in pairs.iter().enumerate() {
        let rec = rel.add(i * 40);
        write_text_cell(rec, name); // name @0
        (rec.add(16) as *mut i64).write(i as i64); // ordinality @16
        write_text_cell(rec.add(24), value); // value @24
    }
    rel
}

/// Build the boxed `RawRequest` record (72 B). The cells are *moved in* (rc = 1,
/// the record owns them), so the host's single `coddl_rc_release` of the record
/// frees all four Texts and the headers relation via the drop walker.
unsafe fn build_raw_request(
    method: &[u8],
    path: &[u8],
    query: &[u8],
    headers: &[(Vec<u8>, Vec<u8>)],
    body: &[u8],
) -> *mut u8 {
    let rec = coddl_rc_alloc(72, 1, CoddlKind::Relation as u32, raw_request_desc());
    write_text_cell(rec, body); // body @0
    (rec.add(16) as *mut *mut u8).write(build_headers(headers)); // headers @16
    write_text_cell(rec.add(24), method); // method @24
    write_text_cell(rec.add(40), path); // path @40
    write_text_cell(rec.add(56), query); // query @56
    rec
}

// ── The handler ───────────────────────────────────────────────────────────

// The compiled Coddl handler (`CODDL_APP_OBJ`): one boxed `RawRequest` pointer
// in, one boxed `RawResponse` pointer out. A compiled module also exports the
// P1b lifecycle functions — `coddl_app_init` (runtime init + database/plan/relvar
// registration) and `coddl_app_shutdown` (releases + runtime shutdown) — which
// the host calls once around the accept loop.
#[cfg(coddl_app_obj)]
extern "C" {
    fn handle(req_ptr: *mut u8) -> *mut u8;
    fn coddl_app_init();
    fn coddl_app_shutdown();
}

/// Runs `coddl_app_init` on construction and `coddl_app_shutdown` on drop, so a
/// graceful exit tears the runtime down once. The accept loop below runs until
/// the process is killed, so in practice shutdown is best-effort — per the
/// lifecycle contract, a skipped shutdown just leaks the connection pool at exit
/// (benign). Only present when a compiled handler is linked.
#[cfg(coddl_app_obj)]
struct AppLifecycle;

#[cfg(coddl_app_obj)]
impl Drop for AppLifecycle {
    fn drop(&mut self) {
        unsafe { coddl_app_shutdown() };
    }
}

/// Built-in default handler when no `CODDL_APP_OBJ` is linked: hand-build a
/// `RawResponse` record (status 200, one `Content-Type` header, body `hello`)
/// with the same layout a compiled handler's boxing emits, so the host's
/// read/release path is identical. Ignores the request. Keeps
/// `cargo run -p coddl-web` self-contained *and* proves the host can construct a
/// record — including an ordinality-carrying headers relation — not just read one.
#[cfg(not(coddl_app_obj))]
unsafe fn handle(_req_ptr: *mut u8) -> *mut u8 {
    build_response(
        200,
        b"hello",
        &[(b"Content-Type".to_vec(), b"text/plain".to_vec())],
    )
}

/// Hand-build a boxed `RawResponse` record `{ body, headers, status }`. Cells are
/// *moved in* (rc = 1); the host's single release frees the body Text and the
/// headers relation (and its cells) via the drop walker. Default-handler only.
#[cfg(not(coddl_app_obj))]
unsafe fn build_response(status: i64, body: &[u8], headers: &[(Vec<u8>, Vec<u8>)]) -> *mut u8 {
    let rec = coddl_rc_alloc(32, 1, CoddlKind::Relation as u32, response_desc());
    write_text_cell(rec, body); // body @0
    (rec.add(16) as *mut *mut u8).write(build_headers(headers)); // headers @16
    (rec.add(24) as *mut i64).write(status); // status @24
    rec
}

// ── Value readers (handler → host) ─────────────────────────────────────────

/// Read a `RawResponse` record: `status` (i64 @24), a **copy** of the `body`
/// bytes (ptr @0, len @8), and a **copy** of the response headers (the
/// `{name, ordinality, value}` relation @16). Everything is copied out *before*
/// the caller releases the record (webhost.md sharp edge #1 — the drop walker
/// frees the body cell and the headers relation on release).
unsafe fn read_response(rec: *mut u8) -> (i64, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>) {
    if rec.is_null() {
        return (500, Vec::new(), Vec::new());
    }
    let status = (rec.add(24) as *const i64).read();
    let body = read_text_cell(rec); // body cell @0
    let headers_ptr = (rec.add(16) as *const *const u8).read();
    let headers = read_headers_relation(headers_ptr);
    (status, body, headers)
}

/// Copy an `OrderedNameValues` relation's records into owned `(name, value)`
/// pairs. Record count is the payload's `CoddlRcHeader.length`; each record is
/// 40 B, `name` cell @0, `value` cell @24 (`ordinality` @16 is skipped — the
/// handler's relation-literal order is preserved as record order here).
unsafe fn read_headers_relation(rel: *const u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    if rel.is_null() {
        return Vec::new();
    }
    let count = (*(rel.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let rec = rel.add(i * 40);
        out.push((read_text_cell(rec), read_text_cell(rec.add(24))));
    }
    out
}

/// Read a `Text` cell `(ptr @0, len @8)` into owned bytes.
unsafe fn read_text_cell(cell: *const u8) -> Vec<u8> {
    let ptr = (cell as *const *const u8).read();
    let len = (cell.add(8) as *const u64).read() as usize;
    if ptr.is_null() || len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(ptr, len).to_vec()
    }
}

// ── HTTP ───────────────────────────────────────────────────────────────────

/// The reason phrase for the status codes this host emits.
fn reason(status: i64) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

/// A parsed HTTP/1.1 request: method + request-target from the request line, the
/// header lines as `(name, value)` pairs, and the body (framed by
/// `Content-Length`). The target is split into raw `path` / `query` at marshal
/// time (see `split_target`).
struct ParsedRequest {
    method: Vec<u8>,
    target: Vec<u8>,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    body: Vec<u8>,
}

/// Read one full HTTP/1.1 request: block until the `\r\n\r\n` header terminator
/// (accumulating across `read`s, so headers split over TCP segments are fine),
/// parse the request line + headers, then read exactly `Content-Length` body
/// bytes. Returns `None` on an empty/closed connection. Chunked transfer and
/// pipelining are out of scope: no `Content-Length` ⇒ no body, and any bytes
/// past `Content-Length` are dropped.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<ParsedRequest>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break buf.len(); // closed before terminator; best-effort parse
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let (method, target, headers) = parse_head(&buf[..head_end]);
    let content_length = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"content-length"))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = (head_end + 4).min(buf.len());
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break; // client closed early; serve what arrived
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(ParsedRequest {
        method,
        target,
        headers,
        body,
    }))
}

/// Parse the head block (everything before `\r\n\r\n`): the request line yields
/// method + request-target; each remaining line is a header split once on `:`.
fn parse_head(head: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>) {
    let mut lines = head.split(|&b| b == b'\n').map(strip_cr);
    let request_line = lines.next().unwrap_or(&[]);
    let mut parts = request_line.split(|&b| b == b' ');
    let method = parts.next().unwrap_or(b"GET").to_vec();
    let target = parts.next().unwrap_or(b"/").to_vec();

    // Header lines: split once on `:`, trim OWS, preserve names as received,
    // and dedup identical `(name, value)` pairs so the relation keeps set
    // semantics (RM Pro 1 — no duplicate tuples).
    let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = trim(&line[..colon]).to_vec();
        let value = trim(&line[colon + 1..]).to_vec();
        if !headers.iter().any(|(n, v)| n == &name && v == &value) {
            headers.push((name, value));
        }
    }
    (method, target, headers)
}

/// Split a request-target into raw `(path, query)` at the first `?` (the one
/// split RFC 3986 defines; both parts stay percent-encoded). No `?` ⇒ empty query.
fn split_target(target: &[u8]) -> (&[u8], &[u8]) {
    match target.iter().position(|&b| b == b'?') {
        Some(q) => (&target[..q], &target[q + 1..]),
        None => (target, &[]),
    }
}

/// Strip a single trailing `\r` (the CR of a CRLF line ending).
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

/// Trim leading/trailing ASCII whitespace (HTTP optional whitespace).
fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
    let end = s
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &s[start..end]
}

/// Framing headers the host computes itself; a handler-provided copy is dropped
/// so it can't conflict with the host's transport management.
fn is_framing_header(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"content-length")
        || name.eq_ignore_ascii_case(b"connection")
        || name.eq_ignore_ascii_case(b"transfer-encoding")
}

/// Marshal a request in, call the handler, read the response out. The request
/// record owns its cells, so releasing it once cascades through the drop walker;
/// the response body/headers are copied out before its single release.
fn invoke_handler(req: &ParsedRequest) -> (i64, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>) {
    let (path, query) = split_target(&req.target);
    unsafe {
        let req_rec = build_raw_request(&req.method, path, query, &req.headers, &req.body);
        let resp_rec = handle(req_rec);
        let out = read_response(resp_rec); // copies body + response headers out

        coddl_rc_release(resp_rec); // frees the response body + headers cells
        coddl_rc_release(req_rec); // frees the request record + all its cells
        out
    }
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let req = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()), // empty / closed connection
    };

    let (status, body, resp_headers) = invoke_handler(&req);

    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    // Handler-provided headers (e.g. Content-Type) — minus the framing headers
    // the host owns, so a handler can't fight transport management.
    for (name, value) in &resp_headers {
        if is_framing_header(name) {
            continue;
        }
        head.push_str(&String::from_utf8_lossy(name));
        head.push_str(": ");
        head.push_str(&String::from_utf8_lossy(value));
        head.push_str("\r\n");
    }
    // Host-owned framing headers.
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

fn main() -> std::io::Result<()> {
    // `CODDL_WEB_ADDR` overrides the bind address; a `:0` port asks the OS for a
    // free one (used by the integration test to avoid port collisions).
    let addr = std::env::var("CODDL_WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8000".to_string());
    let listener = TcpListener::bind(&addr)?;

    // Run the compiled handler's app lifecycle once: `coddl_app_init` registers
    // its database / plans / relvar slots so a `handle` call can touch a relvar;
    // the guard runs `coddl_app_shutdown` on exit. The built-in default handler
    // links no compiled module and touches no relvar, so it needs neither.
    #[cfg(coddl_app_obj)]
    let _lifecycle = unsafe {
        coddl_app_init();
        AppLifecycle
    };

    // Announce the resolved address (port `:0` becomes concrete here) so a
    // supervising process can learn where to connect, then flush immediately.
    println!("coddl-web listening on http://{}", listener.local_addr()?);
    std::io::stdout().flush()?;

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = serve(s) {
                    eprintln!("coddl-web: connection error: {e}");
                }
            }
            Err(e) => eprintln!("coddl-web: accept error: {e}"),
        }
    }
    Ok(())
}
