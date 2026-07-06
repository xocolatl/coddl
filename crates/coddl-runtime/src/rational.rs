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
}
