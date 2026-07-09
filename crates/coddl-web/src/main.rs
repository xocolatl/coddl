//! `coddl-web` — the web host (see docs/webhost.md), P2.
//!
//! A single-threaded HTTP/1.1 listener that owns `main`, parses each request
//! into a Coddl `Request` value, calls a handler across the C ABI, reads the
//! `Response` value back, and writes the HTTP reply. It knows no relational
//! algebra: the handler is an opaque C symbol, exactly the compiler/runtime
//! boundary docs/workspace.md defines. Everything web-specific — sockets, HTTP
//! parsing, and the *hand-written marshalling* of Coddl's RC-headed values — is
//! FFI-bottom "stays Rust" work (docs/principles.md self-hosting fault line).
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
use std::net::TcpListener;

use coddl_runtime::{
    coddl_rc_alloc, coddl_rc_release, CoddlAttrDesc, CoddlAttrKind, CoddlHeadingDesc, CoddlKind,
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

/// An empty `Relation { name, value }` (zero records) — the request `headers`
/// first cut (populated headers are a follow-up). Caller releases it.
unsafe fn empty_headers() -> *mut u8 {
    coddl_rc_alloc(0, 0, CoddlKind::Relation as u32, headers_desc())
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
/// `Response` record (status 200, empty headers, body `hello`) with the same
/// layout a compiled handler's boxing emits, so the host's read/release path is
/// identical. Ignores the request. Keeps `cargo run -p coddl-web` self-contained
/// *and* proves the host can construct a record, not just read one.
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
    build_response(200, b"hello")
}

/// Hand-build a boxed `Response` record `{ body, headers: <empty>, status }`.
/// The cells are *moved in* (rc = 1, the record owns them), so the host's
/// single `coddl_rc_release` of the record frees the body Text and the headers
/// relation via the drop walker. Default-handler only.
#[cfg(not(coddl_app_obj))]
unsafe fn build_response(status: i64, body: &[u8]) -> *mut u8 {
    let rec = coddl_rc_alloc(32, 1, CoddlKind::Relation as u32, response_desc());
    let body_text = rc_text(body);
    (rec as *mut *mut u8).write(body_text); // body ptr @0
    (rec.add(8) as *mut u64).write(body.len() as u64); // body len @8
    (rec.add(16) as *mut *mut u8).write(empty_headers()); // headers @16
    (rec.add(24) as *mut i64).write(status); // status @24
    rec
}

// ── Value readers (handler → host) ─────────────────────────────────────────

/// Read a `Response` record: `status` (i64 @24) and a **copy** of the `body`
/// bytes (ptr @0, len @8). The body is copied out *before* the caller releases
/// the record (webhost.md sharp edge #1 — the drop walker frees the body cell
/// on release, so its bytes would dangle otherwise).
unsafe fn read_response(rec: *mut u8) -> (i64, Vec<u8>) {
    if rec.is_null() {
        return (500, Vec::new());
    }
    let body_ptr = (rec as *const *const u8).read();
    let body_len = (rec.add(8) as *const u64).read() as usize;
    let status = (rec.add(24) as *const i64).read();
    let body = if body_ptr.is_null() || body_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(body_ptr, body_len).to_vec()
    };
    (status, body)
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

/// Minimal HTTP/1.1 parse: `(method, path, body)`. Method and path come from the
/// request line; the body is whatever follows the `\r\n\r\n` header terminator.
/// Real header parsing + routing is P3.
fn parse_request(raw: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(raw.len());
    let line_end = raw.windows(2).position(|w| w == b"\r\n").unwrap_or(0);
    let line = &raw[..line_end];
    let mut parts = line.split(|&b| b == b' ');
    let method = parts.next().unwrap_or(b"GET").to_vec();
    let path = parts.next().unwrap_or(b"/").to_vec();
    let body = raw.get(head_end..).unwrap_or(&[]).to_vec();
    (method, path, body)
}

/// Marshal a request in, call the handler, read the response out — with the
/// three sharp edges: RC-headed request Texts (#3), copy the body before
/// release (#1), release every payload exactly once (#2).
fn invoke_handler(method: &[u8], path: &[u8], body: &[u8]) -> (i64, Vec<u8>) {
    unsafe {
        let m = rc_text(method);
        let p = rc_text(path);
        let b = rc_text(body);
        let h = empty_headers();

        let rec = handle(b, body.len(), h, m, method.len(), p, path.len());
        let (status, out) = read_response(rec); // copies the body out

        coddl_rc_release(rec); // frees the response body + headers cells
        coddl_rc_release(m);
        coddl_rc_release(p);
        coddl_rc_release(b);
        coddl_rc_release(h);
        (status, out)
    }
}

fn serve(mut stream: std::net::TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let (method, path, body) = parse_request(&buf[..n]);

    let (status, out) = invoke_handler(&method, &path, &body);
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        out.len(),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&out)?;
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
