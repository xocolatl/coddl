//! Optionally link a separately-compiled Coddl handler into the host.
//!
//! By default `coddl-web` ships a built-in `hello\n` handler so the binary is
//! self-contained and `cargo run -p coddl-web` serves out of the box. Point
//! `CODDL_APP_OBJ` at an object emitted by `coddl emit-obj <handler>.cd` to
//! link a real compiled Coddl handler instead; its `handle` symbol replaces the
//! built-in one, and its runtime-symbol references are satisfied by the
//! `coddl-runtime` rlib already linked in. This is the deliberate one-app cut of
//! the spine; a driver-driven "one self-contained binary" build is follow-up
//! (see docs/webhost.md Deployment).

fn main() {
    println!("cargo:rustc-check-cfg=cfg(coddl_app_obj)");
    println!("cargo:rerun-if-env-changed=CODDL_APP_OBJ");
    if let Ok(obj) = std::env::var("CODDL_APP_OBJ") {
        if !obj.is_empty() {
            println!("cargo:rustc-cfg=coddl_app_obj");
            println!("cargo:rustc-link-arg={obj}");
            println!("cargo:rerun-if-changed={obj}");
        }
    }
}
