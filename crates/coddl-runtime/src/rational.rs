//! Bounded exact `Rational` runtime helpers.
//!
//! A rational is a **reduced** `(numer: i128, denom: i128)` pair
//! (`gcd(|n|,d) = 1`, `d > 0`, `0 = (0,1)`). Every producer funnels through
//! [`reduce`]; division by zero and i128 overflow **trap** (a bounded exact
//! type must error, never wrap or silently lose precision).
//!
//! Results cross the C ABI via out-pointers (`out_num`, `out_den`), the same
//! two-slot shape the codegen builds — a Rational is a compound value, like a
//! `Text` `(ptr, len)` pair.

/// Abort with a clear message — the runtime's fail-fast convention for a
/// contract that can't be honored (here: an out-of-range or undefined result).
fn trap(msg: &str) -> ! {
    eprintln!("coddl: {msg}");
    std::process::abort();
}

/// Greatest common divisor of two magnitudes (Euclid).
fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Reduce `(n, d)` to the canonical `Rational`: `gcd(|n|,d) = 1`, `d > 0`,
/// `0 → (0,1)`. Traps on `d == 0` (division by zero is not a rational).
pub(crate) fn reduce(n: i128, d: i128) -> (i128, i128) {
    if d == 0 {
        trap("rational: division by zero");
    }
    if n == 0 {
        return (0, 1);
    }
    let g = gcd(n.unsigned_abs(), d.unsigned_abs()) as i128;
    let (mut n, mut d) = (n / g, d / g);
    if d < 0 {
        n = -n;
        d = -d;
    }
    (n, d)
}

/// Multiply, trapping on i128 overflow (a bounded exact type errors rather than
/// wrapping).
fn mul(a: i128, b: i128) -> i128 {
    a.checked_mul(b)
        .unwrap_or_else(|| trap("rational: overflow"))
}

/// Add, trapping on overflow.
fn add(a: i128, b: i128) -> i128 {
    a.checked_add(b)
        .unwrap_or_else(|| trap("rational: overflow"))
}

/// Exact division of two `Integer`s (surface `/`) → a reduced `Rational`.
/// Widening `i64 → i128` never overflows; `reduce` traps on a zero divisor.
///
/// # Safety
/// `out_num` and `out_den` must point at writable `i128` slots.
#[no_mangle]
pub unsafe extern "C" fn coddl_rational_from_ints(
    a: i64,
    b: i64,
    out_num: *mut i128,
    out_den: *mut i128,
) {
    let (n, d) = reduce(a as i128, b as i128);
    *out_num = n;
    *out_den = d;
}

/// `Integer → Rational` widening: `a → (a, 1)`. (`to_rational` on an Integer.)
///
/// # Safety
/// `out_num`/`out_den` must point at writable `i128` slots.
#[no_mangle]
pub unsafe extern "C" fn coddl_rational_from_int(a: i64, out_num: *mut i128, out_den: *mut i128) {
    *out_num = a as i128;
    *out_den = 1;
}

macro_rules! rational_binop {
    ($name:ident, $num:expr, $den:expr, $doc:literal) => {
        #[doc = $doc]
        ///
        /// # Safety
        /// `out_num`/`out_den` must point at writable `i128` slots.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            n1: i128,
            d1: i128,
            n2: i128,
            d2: i128,
            out_num: *mut i128,
            out_den: *mut i128,
        ) {
            let num = $num(n1, d1, n2, d2);
            let den = $den(n1, d1, n2, d2);
            let (n, d) = reduce(num, den);
            *out_num = n;
            *out_den = d;
        }
    };
}

// `a/b + c/d = (ad + bc)/(bd)`; overflow / zero-denominator trap.
rational_binop!(
    coddl_rational_add,
    |n1, d1, n2, d2| add(mul(n1, d2), mul(n2, d1)),
    |_, d1, _, d2| mul(d1, d2),
    "Rational `+`."
);
rational_binop!(
    coddl_rational_sub,
    |n1, d1, n2, d2| add(mul(n1, d2), -mul(n2, d1)),
    |_, d1, _, d2| mul(d1, d2),
    "Rational `-`."
);
rational_binop!(
    coddl_rational_mul,
    |n1, _, n2, _| mul(n1, n2),
    |_, d1, _, d2| mul(d1, d2),
    "Rational `*`."
);
// `(a/b) / (c/d) = (ad)/(bc)`; `reduce` traps if the divisor is `0` (bc == 0).
rational_binop!(
    coddl_rational_div,
    |n1, _, _, d2| mul(n1, d2),
    |_, d1, n2, _| mul(d1, n2),
    "Rational `/` (exact division)."
);

/// `Rational → Approximate`: the correctly-rounded `f64` value of `n/d`. Uses a
/// widening `i128 → f64` on each component; for components beyond f64's exact
/// range this rounds (the whole point of `to_approximate`). Returns the raw f64.
#[no_mangle]
pub extern "C" fn coddl_rational_to_approx(num: i128, den: i128) -> f64 {
    num as f64 / den as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_canonicalizes() {
        assert_eq!(reduce(34, 10), (17, 5));
        assert_eq!(reduce(6, -4), (-3, 2)); // sign moves to the numerator
        assert_eq!(reduce(0, 7), (0, 1));
        assert_eq!(reduce(-2, -4), (1, 2));
    }

    #[test]
    fn from_ints_reduces() {
        let (mut n, mut d) = (0i128, 0i128);
        unsafe { coddl_rational_from_ints(1, 3, &mut n, &mut d) };
        assert_eq!((n, d), (1, 3));
        unsafe { coddl_rational_from_ints(6, 4, &mut n, &mut d) };
        assert_eq!((n, d), (3, 2));
    }

    fn run(
        f: unsafe extern "C" fn(i128, i128, i128, i128, *mut i128, *mut i128),
        a: (i128, i128),
        b: (i128, i128),
    ) -> (i128, i128) {
        let (mut n, mut d) = (0i128, 0i128);
        unsafe { f(a.0, a.1, b.0, b.1, &mut n, &mut d) };
        (n, d)
    }

    #[test]
    fn arithmetic_reduces() {
        assert_eq!(run(coddl_rational_add, (1, 2), (1, 3)), (5, 6));
        assert_eq!(run(coddl_rational_sub, (3, 4), (1, 4)), (1, 2));
        assert_eq!(run(coddl_rational_mul, (1, 2), (2, 3)), (1, 3));
        assert_eq!(run(coddl_rational_div, (1, 2), (3, 4)), (2, 3));
    }

    #[test]
    fn to_approx_rounds() {
        assert_eq!(coddl_rational_to_approx(1, 2), 0.5);
        assert_eq!(coddl_rational_to_approx(3, 4), 0.75);
    }
}
