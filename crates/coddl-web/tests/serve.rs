//! Integration test for the `coddl-web` host loop.
//!
//! Spawns the built `coddl-web` binary on an OS-chosen ephemeral port
//! (`CODDL_WEB_ADDR=127.0.0.1:0`, so parallel test runs never collide), learns
//! the port from the `listening on …` line it prints, makes one HTTP request,
//! and asserts the response is a `200 OK` carrying the handler's body.
//!
//! This exercises the whole host loop end-to-end — accept, parse the request,
//! build a boxed `RawRequest` value, call the handler across the C ABI, read the
//! boxed `RawResponse` record back, allocate/release every payload through the
//! runtime, write the response — against the built-in `hello` handler, so it
//! needs no compiled app object. The cross-object FFI boundary (a
//! separately-compiled Coddl handler linked into a foreign host) is proved
//! hermetically by the driver e2e test
//! `web_spine_mainless_handler_links_into_c_host`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

/// Kills the child server when the test ends (including on panic), so a failed
/// assertion never leaves a listener process behind.
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn coddl_web_serves_handler_body_over_http() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coddl-web"))
        .env("CODDL_WEB_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn coddl-web");

    // The server prints "coddl-web listening on http://127.0.0.1:PORT" once bound.
    let stdout = child.stdout.take().expect("child stdout");
    let server = Server(child);
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read listening line");
    let addr = line
        .trim()
        .split("http://")
        .nth(1)
        .unwrap_or_else(|| panic!("no address in startup line: {line:?}"))
        .to_string();

    let mut stream = TcpStream::connect(&addr).expect("connect to coddl-web");
    // The request carries headers (`Host`, `X-Test`) so the host's request-header
    // build path runs — parse them into a populated `{name, value}` relation,
    // marshal it across the ABI, and release it. The default handler ignores them.
    stream
        .write_all(
            b"GET /users HTTP/1.1\r\nHost: localhost\r\nX-Test: v\r\nConnection: close\r\n\r\n",
        )
        .expect("send request");
    // The server sets `Connection: close`, so it closes after one response and
    // `read_to_end` terminates.
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).expect("read response");

    // The built-in handler hand-builds a `RawResponse` record (status 200, one
    // `Content-Type` header, body `hello`) that the host reads back over the C
    // ABI. The `Content-Type` line proves the response-header **read** path (the
    // host walked the returned `{name, ordinality, value}` relation into the
    // reply). The body has no trailing newline — it is the record's `body` Text.
    let text = String::from_utf8_lossy(&resp);
    assert!(text.starts_with("HTTP/1.1 200 OK"), "response head: {text:?}");
    assert!(
        text.contains("Content-Type: text/plain\r\n"),
        "expected the handler's Content-Type header, got: {text:?}"
    );
    assert!(
        text.ends_with("\r\n\r\nhello"),
        "expected body `hello`, got: {text:?}"
    );

    drop(server); // explicit: stop the server before the test returns
}
