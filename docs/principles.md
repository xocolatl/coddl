# Coddl — core principles

Four principles, binding on every design choice in the project. A proposal that violates one needs an explicit override, not a quiet exception. These are the lens every other doc applies; when a topic doc says "this is the right shape because…" the answer usually traces back here.

## 1. Performance

Runtime cost is a first-class concern. The host language (Rust), the runtime (no GC, no managed RTS in user binaries), the FFI layer (zero-copy `#[repr(C)]` values), and the IR (Algebra A — push-down-friendly) are chosen for it. Features that force unavoidable overhead the user can't opt out of are rejected. When two designs are otherwise equivalent, the one with the lower steady-state cost wins.

## 2. Long-term planning

IR shapes, type representations, and crate boundaries are designed so deferred Manifesto features (VSS 7 heading polymorphism, transition constraints, type inheritance) and unanticipated extensions land without a rewrite. No painting into corners — keep the data structures wider than current need, and the boundaries semantic rather than expedient.

## 3. Conformance over convenience

When TTM prescribes a behavior, Coddl ships it — even when a non-conforming shortcut would be easier. Sanctioned design freedoms are enumerated in [conformance.md](conformance.md) and bounded there. Anything that looks like a new design freedom needs an explicit add to that list, not an off-the-cuff exception.

## 4. Few primitives, layered sugar

Algebra A core operators (see [relir.md](relir.md)); operators-as-relations; no special cases. Surface sugar — `extend`, `where`, `summarize` — desugars during lowering. Sugar lives in one place, not woven through the IR.

## Coddl is its own D

Tutorial D is the Manifesto's reference D, useful as a study aid and prior-art benchmark, not a spec Coddl follows. Where TTM prescribes behavior, Coddl conforms. Where TTM is silent, Coddl picks the answer aligned with the four principles above — convergence with Tutorial D's specific choice is incidental, not a goal.

The sanctioned design freedoms (host language, surface syntax, evaluation strategy, IR choice) are enumerated in [conformance.md](conformance.md) and bounded there.
