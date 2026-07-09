//! `coddl-web` — the web host (see docs/webhost.md), P3a.
//!
//! A single-threaded HTTP/1.1 listener that owns `main`, parses each request
//! into a Coddl `Request` value, calls a handler across the C ABI, reads the
//! `Response` value back, and writes the HTTP reply. It knows no relational
//! algebra and **no routing** — it calls one handler entry (`handle`) per app
//! as an opaque C symbol and lets the app route in Coddl (docs/webhost.md
//! "Design note: coddl::web is vocabulary, not a framework"). Everything
//! web-specific — sockets, HTTP parsing, and the *hand-written marshalling* of
//! Coddl's RC-headed values — is FFI-bottom "stays Rust" work.
//!
//! As of P3a the boundary carries headers **both ways**: incoming request
//! headers are parsed into the `Request`'s `{name, value}` relation, and the
//! `Response`'s headers relation is read back and emitted into the reply. The
//! body is framed by `Content-Length`.
//!
//! The handler is `use module coddl::web; oper handle { req: Request } ->
//! Response`. Its ABI (confirmed via `coddl emit-llvm`):
//!   - `Request` (56 B) is a *flattened* tuple param — name-sorted attrs
//!     `body, headers, method, path` ⇒ 7 args:
//!     `(body_ptr, body_len, headers_ptr, method_ptr, method_len, path_ptr,
//!     path_len)`. `Text` is a `(ptr, len)` pair; `headers: Relation` is one
//!     RC payload pointer.
//!   - `Response` (32 B) is returned **boxed** — one pointer to a length-1
//!     record, layout `body@0 (ptr@0,len@8), headers@16 (ptr), status@24 (i64)`.
//! The surface name is the linkage name (no mangling). By default the built-in
//! handler below supplies it; `CODDL_APP_OBJ` (see build.rs) links a compiled
//! Coddl handler in its place.

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

/// `{ name: Text, value: Text }` — the `headers` relation heading (name @0,
/// value @16; each `Text` is 16 B). Identical to the `coddl::env` descriptor.
fn headers_desc() -> *const CoddlHeadingDesc {
    use std::sync::OnceLock;
    static DESC: OnceLock<usize> = OnceLock::new();
    *DESC.get_or_init(|| {
        let attrs: &'static [CoddlAttrDesc; 2] = Box::leak(Box::new([
            CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"value".as_ptr(),
                name_len: 5,
                kind: CoddlAttrKind::Text as u32,
                offset: 16,
                sub: std::ptr::null(),
            },
        ]));
        let desc: &'static CoddlHeadingDesc = Box::leak(Box::new(CoddlHeadingDesc {
            attr_count: 2,
            record_size: 32,
            attrs: attrs.as_ptr(),
        }));
        desc as *const CoddlHeadingDesc as usize
    }) as *const CoddlHeadingDesc
}

/// The `Response` record heading `{ body: Text @0, headers: Relation @16,
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
/// if it stores the value (webhost.md sharp edge #3). Caller releases it.
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

/// Build an owned `Relation { name: Text, value: Text }` of `pairs` — one record
/// per pair, each cell a real `rc_text` (rc=1, **moved in**). Mirrors
/// `coddl_env_snapshot` (crates/coddl-runtime/src/env.rs). The caller releases
/// the relation once; the drop walker then frees every cell. `build_headers(&[])`
/// is the empty-relation case (zero records). Caller releases it.
unsafe fn build_headers(pairs: &[(Vec<u8>, Vec<u8>)]) -> *mut u8 {
    let n = pairs.len();
    let rel = coddl_rc_alloc(n * 32, n as u32, CoddlKind::Relation as u32, headers_desc());
    for (i, (name, value)) in pairs.iter().enumerate() {
        let rec = rel.add(i * 32);
        (rec as *mut *mut u8).write(rc_text(name)); // name ptr @0
        (rec.add(8) as *mut u64).write(name.len() as u64); // name len @8
        (rec.add(16) as *mut *mut u8).write(rc_text(value)); // value ptr @16
        (rec.add(24) as *mut u64).write(value.len() as u64); // value len @24
    }
    rel
}

// ── The handler ───────────────────────────────────────────────────────────

// The compiled Coddl handler (`CODDL_APP_OBJ`): flattened `Request` args in,
// boxed `Response` record pointer out.
#[cfg(coddl_app_obj)]
extern "C" {
    fn handle(
        body_ptr: *const u8,
        body_len: usize,
        headers_ptr: *mut u8,
        method_ptr: *const u8,
        method_len: usize,
        path_ptr: *const u8,
        path_len: usize,
    ) -> *mut u8;
}

/// Built-in default handler when no `CODDL_APP_OBJ` is linked: hand-build a
/// `Response` record (status 200, one `Content-Type` header, body `hello`) with
/// the same layout a compiled handler's boxing emits, so the host's
/// read/release path is identical. Ignores the request. Keeps
/// `cargo run -p coddl-web` self-contained *and* proves the host can construct a
/// record — including a populated headers relation — not just read one.
#[cfg(not(coddl_app_obj))]
unsafe fn handle(
    _body_ptr: *const u8,
    _body_len: usize,
    _headers_ptr: *mut u8,
    _method_ptr: *const u8,
    _method_len: usize,
    _path_ptr: *const u8,
    _path_len: usize,
) -> *mut u8 {
    build_response(
        200,
        b"hello",
        &[(b"Content-Type".to_vec(), b"text/plain".to_vec())],
    )
}

/// Hand-build a boxed `Response` record `{ body, headers, status }`. The cells
/// are *moved in* (rc = 1, the record owns them), so the host's single
/// `coddl_rc_release` of the record frees the body Text and the headers relation
/// (and its cells) via the drop walker. Default-handler only.
#[cfg(not(coddl_app_obj))]
unsafe fn build_response(status: i64, body: &[u8], headers: &[(Vec<u8>, Vec<u8>)]) -> *mut u8 {
    let rec = coddl_rc_alloc(32, 1, CoddlKind::Relation as u32, response_desc());
    (rec as *mut *mut u8).write(rc_text(body)); // body ptr @0
    (rec.add(8) as *mut u64).write(body.len() as u64); // body len @8
    (rec.add(16) as *mut *mut u8).write(build_headers(headers)); // headers @16
    (rec.add(24) as *mut i64).write(status); // status @24
    rec
}

// ── Value readers (handler → host) ─────────────────────────────────────────

/// Read a `Response` record: `status` (i64 @24), a **copy** of the `body` bytes
/// (ptr @0, len @8), and a **copy** of the response headers (the `{name, value}`
/// relation @16). Everything is copied out *before* the caller releases the
/// record (webhost.md sharp edge #1 — the drop walker frees the body cell and
/// the headers relation on release, so they would dangle otherwise).
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

/// Copy a `{ name: Text, value: Text }` relation's records into owned pairs. The
/// record count is the payload's `CoddlRcHeader.length` (32-byte header before
/// the payload); each record is 32 B, `name` cell @0, `value` cell @16.
unsafe fn read_headers_relation(rel: *const u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    if rel.is_null() {
        return Vec::new();
    }
    let count = (*(rel.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let rec = rel.add(i * 32);
        out.push((read_text_cell(rec), read_text_cell(rec.add(16))));
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

/// A parsed HTTP/1.1 request: method + path from the request line, the header
/// lines as `(name, value)` pairs, and the body (framed by `Content-Length`).
struct ParsedRequest {
    method: Vec<u8>,
    path: Vec<u8>,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    body: Vec<u8>,
}

/// Read one full HTTP/1.1 request: block until the `\r\n\r\n` header terminator
/// (accumulating across `read`s, so headers split over TCP segments are fine),
/// parse the request line + headers, then read exactly `Content-Length` body
/// bytes. Returns `None` on an empty/closed connection. Chunked transfer and
/// pipelining are out of scope (P3): no `Content-Length` ⇒ no body, and any
/// bytes past `Content-Length` are dropped.
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

    let (method, path, headers) = parse_head(&buf[..head_end]);
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
        path,
        headers,
        body,
    }))
}

/// Parse the head block (everything before `\r\n\r\n`): the request line yields
/// method + path; each remaining line is a header split once on `:`.
fn parse_head(head: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>) {
    let mut lines = head.split(|&b| b == b'\n').map(strip_cr);
    let request_line = lines.next().unwrap_or(&[]);
    let mut parts = request_line.split(|&b| b == b' ');
    let method = parts.next().unwrap_or(b"GET").to_vec();
    let path = parts.next().unwrap_or(b"/").to_vec();

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
    (method, path, headers)
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

/// Marshal a request in, call the handler, read the response out — with the
/// three sharp edges: RC-headed request Texts + headers (#3), copy body and
/// response headers before release (#1), release every payload exactly once (#2).
fn invoke_handler(
    method: &[u8],
    path: &[u8],
    headers: &[(Vec<u8>, Vec<u8>)],
    body: &[u8],
) -> (i64, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>) {
    unsafe {
        let m = rc_text(method);
        let p = rc_text(path);
        let b = rc_text(body);
        let h = build_headers(headers);

        let rec = handle(b, body.len(), h, m, method.len(), p, path.len());
        let out = read_response(rec); // copies body + response headers out

        coddl_rc_release(rec); // frees the response body + headers cells
        coddl_rc_release(m);
        coddl_rc_release(p);
        coddl_rc_release(b);
        coddl_rc_release(h); // frees the request headers relation + its cells
        out
    }
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let req = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()), // empty / closed connection
    };

    let (status, body, resp_headers) =
        invoke_handler(&req.method, &req.path, &req.headers, &req.body);

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
