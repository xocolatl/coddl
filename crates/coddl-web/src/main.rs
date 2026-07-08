//! `coddl-web` — the web host spine (see docs/webhost.md).
//!
//! A single-threaded HTTP/1.1 listener that owns `main`, calls a Coddl handler
//! across the C ABI, writes a fixed `200 OK`, and releases the returned payload.
//! It knows no relational algebra: the handler is an opaque C symbol, exactly
//! the compiler/runtime boundary docs/workspace.md defines. This is the spine —
//! it ignores the request and hardcodes status/headers; `Request`/`Response`
//! marshalling and real routing (webhost.md P2/P3) come later.
//!
//! The handler is `oper handle {} -> Text`: its Text return uses the fat-pointer
//! convention — one trailing length-out pointer, the payload pointer returned —
//! and the surface name is the linkage name (no mangling). By default the
//! built-in handler below supplies it; `CODDL_APP_OBJ` (see build.rs) links a
//! compiled Coddl handler in its place.

use std::io::{Read, Write};
use std::net::TcpListener;

// The Coddl handler. With `CODDL_APP_OBJ` set, this is the `handle` symbol from
// the linked-in compiled object; the length-out pointer receives the body byte
// length and the return value is the payload pointer.
#[cfg(coddl_app_obj)]
extern "C" {
    fn handle(ret_len_out: *mut usize) -> *mut u8;
}

/// Built-in handler used when no `CODDL_APP_OBJ` is linked. Returns a real
/// heap `Text` (`hello\n`) allocated through the runtime, so the host's uniform
/// `coddl_rc_release` frees it exactly as it would a compiled handler's payload.
#[cfg(not(coddl_app_obj))]
unsafe fn handle(ret_len_out: *mut usize) -> *mut u8 {
    use coddl_runtime::{coddl_rc_alloc, CoddlKind};
    let body = b"hello\n";
    let p = coddl_rc_alloc(
        body.len(),
        body.len() as u32,
        CoddlKind::Text as u32,
        std::ptr::null(),
    );
    if p.is_null() {
        *ret_len_out = 0;
        return p;
    }
    std::ptr::copy_nonoverlapping(body.as_ptr(), p, body.len());
    *ret_len_out = body.len();
    p
}

/// Call the handler across the C ABI and return the response body. The bytes are
/// copied out *before* the payload is released — with the relation return of P2
/// the body is a cell inside the record, and the drop walker frees it when the
/// outer payload is released, so copy-first is the discipline (webhost.md sharp
/// edge #1). `coddl_rc_release` no-ops on immortal literals, so it is always
/// safe to release exactly once.
fn invoke_handler() -> Vec<u8> {
    unsafe {
        let mut len: usize = 0;
        let payload = handle(&mut len);
        let body = if payload.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(payload, len).to_vec()
        };
        coddl_runtime::coddl_rc_release(payload);
        body
    }
}

fn serve(mut stream: std::net::TcpStream) -> std::io::Result<()> {
    // Spine: read and discard the request. HTTP parsing arrives with P3.
    let mut scratch = [0u8; 1024];
    let _ = stream.read(&mut scratch);

    let body = invoke_handler();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
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
