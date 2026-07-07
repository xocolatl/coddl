//! Bounded exact `Rational` runtime helpers.
//!
//! A rational is a **reduced** `(numer: i64, denom: i64)` pair
//! (`gcd(|n|,d) = 1`, `d > 0`, `0 = (0,1)`). Every producer funnels through
//! [`reduce_to_i64`]; division by zero and overflow of the reduced `i64`
//! components **trap** (a bounded exact type must error, never wrap or silently
//! lose precision). Arithmetic runs in `i128` intermediates so a cross-multiply
//! never wraps *before* reduction — only a result that stays too big *after*
//! reduction traps.
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

/// Reduce an `i128` fraction to the canonical `i64` `Rational`: `gcd(|n|,d) = 1`,
/// `d > 0`, `0 → (0,1)`. Traps on `d == 0` (division by zero is not a rational)
/// and on a reduced component that no longer fits `i64` (the bounded type's
/// ceiling — error rather than wrap).
pub(crate) fn reduce_to_i64(n: i128, d: i128) -> (i64, i64) {
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
    let narrow = |v: i128| i64::try_from(v).unwrap_or_else(|_| trap("rational: overflow"));
    (narrow(n), narrow(d))
}

/// Multiply in `i128`, trapping on overflow (a bounded exact type errors rather
/// than wrapping). Operands are `i64`-widened, so a single product never
/// overflows; this guards the pathological chained case.
fn mul(a: i128, b: i128) -> i128 {
    a.checked_mul(b)
        .unwrap_or_else(|| trap("rational: overflow"))
}

/// Add in `i128`, trapping on overflow.
fn add(a: i128, b: i128) -> i128 {
    a.checked_add(b)
        .unwrap_or_else(|| trap("rational: overflow"))
}

/// Exact division of two `Integer`s (surface `/`) → a reduced `Rational`.
/// `reduce_to_i64` traps on a zero divisor; the reduced pair always fits `i64`
/// here (both inputs already do).
///
/// # Safety
/// `out_num` and `out_den` must point at writable `i64` slots.
#[no_mangle]
pub unsafe extern "C" fn coddl_rational_from_ints(
    a: i64,
    b: i64,
    out_num: *mut i64,
    out_den: *mut i64,
) {
    let (n, d) = reduce_to_i64(a as i128, b as i128);
    *out_num = n;
    *out_den = d;
}

macro_rules! rational_binop {
    ($name:ident, $num:expr, $den:expr, $doc:literal) => {
        #[doc = $doc]
        ///
        /// # Safety
        /// `out_num`/`out_den` must point at writable `i64` slots.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            n1: i64,
            d1: i64,
            n2: i64,
            d2: i64,
            out_num: *mut i64,
            out_den: *mut i64,
        ) {
            // Widen to i128 so the cross-multiply can't wrap before reduction.
            let (n1, d1, n2, d2) = (n1 as i128, d1 as i128, n2 as i128, d2 as i128);
            let num = $num(n1, d1, n2, d2);
            let den = $den(n1, d1, n2, d2);
            let (n, d) = reduce_to_i64(num, den);
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

/// `Rational → Approximate`: the correctly-rounded `f64` value of `n/d` via an
/// `i64 → f64` widening on each component (each is exactly representable up to
/// f64's 53-bit mantissa; the division then rounds, which is the point of
/// `to_approximate`). Returns the raw f64.
#[no_mangle]
pub extern "C" fn coddl_rational_to_approx(num: i64, den: i64) -> f64 {
    num as f64 / den as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_canonicalizes() {
        assert_eq!(reduce_to_i64(34, 10), (17, 5));
        assert_eq!(reduce_to_i64(6, -4), (-3, 2)); // sign moves to the numerator
        assert_eq!(reduce_to_i64(0, 7), (0, 1));
        assert_eq!(reduce_to_i64(-2, -4), (1, 2));
    }

    #[test]
    fn from_ints_reduces() {
        let (mut n, mut d) = (0i64, 0i64);
        unsafe { coddl_rational_from_ints(1, 3, &mut n, &mut d) };
        assert_eq!((n, d), (1, 3));
        unsafe { coddl_rational_from_ints(6, 4, &mut n, &mut d) };
        assert_eq!((n, d), (3, 2));
    }

    fn run(
        f: unsafe extern "C" fn(i64, i64, i64, i64, *mut i64, *mut i64),
        a: (i64, i64),
        b: (i64, i64),
    ) -> (i64, i64) {
        let (mut n, mut d) = (0i64, 0i64);
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
    fn arithmetic_uses_wide_intermediates() {
        // `1/4e9 + 1/4e9`: the intermediate denominator `4e9 * 4e9 = 1.6e19`
        // overflows i64 (i64::MAX ≈ 9.22e18), but the reduced result `1/2e9`
        // fits — so it must succeed. The i128 intermediate is what saves it;
        // an i64 cross-multiply would have wrapped/trapped before reduction.
        let (n, d) = run(coddl_rational_add, (1, 4_000_000_000), (1, 4_000_000_000));
        assert_eq!((n, d), (1, 2_000_000_000));
    }

    #[test]
    fn to_approx_rounds() {
        assert_eq!(coddl_rational_to_approx(1, 2), 0.5);
        assert_eq!(coddl_rational_to_approx(3, 4), 0.75);
    }
}
