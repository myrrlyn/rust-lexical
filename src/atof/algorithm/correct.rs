//! Correct algorithms for string-to-float conversions.
//!
//! This implementation is loosely based off the Golang implementation,
//! found here:
//!     https://golang.org/src/strconv/atof.go
//!
//! The extended-precision and decimal versions are highly
// Fix a compiler bug that thinks `ExactExponent` isn't used.
#![allow(unused_imports)]

use atoi;
use float::{FloatRounding, FloatType};
use table::*;
use util::*;
use super::cached;
use super::exponent::parse_exponent;

// SHARED

// Fast path for the parse algorithm.
// In this case, the mantissa can be represented by an integer,
// which allows any value to be exactly reconstructed.


// PARSE
// -----

/// Safely convert the number of bits truncated to an exponent.
#[inline]
fn usize_to_i32(truncated: usize) -> i32 {
    const MAX: usize = i32::max_value() as usize;
    if truncated < MAX  {
        truncated as i32
    } else {
        i32::max_value()
    }
}

/// Parse the mantissa from a string.
///
/// Returns the mantissa, the shift in the mantissa relative to the dot,
/// a pointer to the current buffer position, and if the mantissa was
/// truncated.
///
/// The float string must be non-special, non-zero, and positive.
#[inline]
pub(super) unsafe extern "C" fn parse_mantissa(base: u32, mut first: *const u8, last: *const u8)
    -> (u64, i32, *const u8, bool)
{
    // Trim the leading 0s.
    // Need to force this here, since if not, conversion of usize dot to
    // i32 may truncate when mantissa does not, which would lead to faulty
    // results. If we trim the 0s here, we guarantee any time `dot as i32`
    // leads to a truncation, mantissa will overflow.
    first = ltrim_char(first, last, b'0');

    // Parse the integral value.
    // Use the checked parsers so the truncated value is valid even if
    // the entire value is not parsed.
    let mut mantissa: u64 = 0;
    let (f, truncated) = atoi::checked(&mut mantissa, base, first, last);

    // Check for trailing digits
    let has_fraction = distance(f, last) > 1 && *f == b'.';
    if has_fraction && truncated == 0 {
        // Has a decimal, calculate the rest of it.
        let f = f.add(1);
        let tup = match mantissa {
            0 => {
                // Can ignore the leading digits while the mantissa is 0.
                // This allows us to represent extremely small values
                // using the fast route in non-scientific notation.
                // For example, this allows us to use the fast path for
                // both "1e-29" and "0.0000000000000000000000000001",
                // otherwise, only the former would work.
                let f = ltrim_char(f, last, b'0');
                atoi::checked(&mut mantissa, base, f, last)
            },
            _ => atoi::checked(&mut mantissa, base, f, last),
        };
        // Subtract the number of truncated digits from the dot shift, since these
        // truncated digits are reflected in the distance but not in the mantissa.
        let dot_shift = usize_to_i32(distance(f, tup.0)) - usize_to_i32(tup.1);
        (mantissa, dot_shift, tup.0, tup.1 != 0)
    } else if has_fraction {
        // Integral overflow occurred, cannot add more values, but a fraction exists.
        // Ignore the remaining characters, but factor them into the dot exponent.
        let f = f.add(1);
        let mut p = f;
        while p < last && (char_to_digit(*p) as u32) < base {
            p = p.add(1);
        }
        // Subtract the number of truncated digits from the dot shift, since these
        // truncated digits are reflected in the distance but not in the mantissa.
        let dot_shift = usize_to_i32(distance(f, p)) - usize_to_i32(truncated);
        (mantissa, dot_shift, p, true)
    } else {
        // No decimal, just return, noting if truncation occurred.
        // Any truncated digits did not increase the mantissa, make dot_shift
        // negative to compensate.
        let dot_shift = -usize_to_i32(truncated);
        (mantissa, dot_shift, f, truncated != 0)
    }
}

/// Calculate the exact exponent without overflow.
///
/// Remove the number of digits that contributed to the mantissa past
/// the dot.
#[inline]
pub(super) extern "C" fn normalize_exponent(exponent: i32, dot_shift: i32)
    -> i32
{
    match exponent {
         0x7FFFFFFF => i32::max_value(),
        -0x80000000 => i32::min_value(),
        _           => exponent - dot_shift,
    }
}

/// Normalize the mantissa to check if it can use the fast-path.
///
/// Move digits from the mantissa to the exponent when possible.
#[inline]
pub(super) extern "C" fn normalize_mantissa(mut mantissa: u64, base: u32, mut exponent: i32)
    -> (u64, i32)
{
    let base: u64 = as_(base);
    let base2 = base * base;
    let base4 = base2 * base2;

    // Use power-reduction, we're likely never going to enter most of these
    // loops, but it minimizes the number of expensive operations we need
    // to do.
    while mantissa >= base4 && mantissa % base4 == 0 {
        mantissa /= base4;
        exponent += 4;
    }
    while mantissa >= base2 && mantissa % base2 == 0 {
        mantissa /= base2;
        exponent += 2;
    }
    if mantissa % base == 0 {
        mantissa /= base;
        exponent += 1;
    }
    (mantissa, exponent)
}

/// Parse the mantissa and exponent from a string.
///
/// Returns the mantissa, the exponent, number of digits since the dot
/// was seen, a pointer to the current buffer position, and if mantissa
/// was truncated.
///
/// The number of digits ignored relative to the dot may be positive
/// (digits past the dot added to the mantissa) or negative (truncated
/// digits from the integer component).
///
/// The float string must be non-special, non-zero, and positive.
#[inline]
unsafe extern "C" fn parse_float(base: u32, first: *const u8, last: *const u8)
    -> (u64, i32, *const u8, bool)
{
    let (mantissa, dot_shift, p, truncated) = parse_mantissa(base, first, last);
    let (exponent, p) = parse_exponent(base, p, last);
    let exponent = normalize_exponent(exponent, dot_shift);
    let (mantissa, exponent) = normalize_mantissa(mantissa, base, exponent);
    (mantissa, exponent, p, truncated)
}

// EXACT
// -----

/// Check if value is power of 2 and get the power.
#[inline]
fn pow2_exponent(base: u32) -> i32 {
    match base {
        2  => 1,
        4  => 2,
        8  => 3,
        16 => 4,
        32 => 5,
        _  => 0,
    }
}

/// Convert power-of-two to exact value.
///
/// This works since multiplying by the exponent will not affect the
/// mantissa unless the exponent is denormal, which will cause truncation
/// regardless.
#[inline]
fn pow2_to_exact<F: StablePower>(mantissa: u64, base: u32, pow2_exp: i32, exponent: i32)
    -> (F, bool)
{
    debug_assert!(pow2_exp != 0, "Not a power of 2.");

    // As long as the value is within the bounds, we can get an exact value.
    // Since any power of 2 only affects the exponent, we should be able to get
    // any exact value.

    // We know that if any value is > than max_exp, we get infinity, since
    // the mantissa must be positive. We know that the actual value that
    // causes underflow is 64, use 65 since that prevents inaccurate
    // rounding for any pow2_exp.
    let (min_exp, max_exp) = F::exponent_limit(base);
    let underflow_exp = min_exp - (65 / pow2_exp);
    if exponent > max_exp {
        (F::INFINITY, true)
    } else if exponent < underflow_exp{
        (F::ZERO, true)
    } else if exponent < min_exp {
        // We know the mantissa is somewhere <= 65 below min_exp.
        // May still underflow, but it's close. Use the first multiplication
        // which guarantees no truncation, and then the second multiplication
        // which will round to the accurate representation.
        let remainder = exponent - min_exp;
        let float: F = as_(mantissa);
        let float = unsafe { float.pow2(pow2_exp * remainder).pow2(pow2_exp * min_exp) };
        (float, true)
    } else {
        let float: F = as_(mantissa);
        let float = unsafe { float.pow2(pow2_exp * exponent) };
        (float, true)
    }
}


/// Convert mantissa to exact value for a non-base2 power.
///
/// Returns the resulting float and if the value can be represented exactly.
#[inline]
fn to_exact<F: StablePower>(mantissa: u64, base: u32, exponent: i32) -> (F, bool)
{
    // logic error, disable in release builds
    debug_assert!(base >= 2 && base <= 36, "Numerical base must be from 2-36");
    debug_assert!(pow2_exponent(base) == 0, "Cannot use `to_exact` with a power of 2.");

    let (min_exp, max_exp) = F::exponent_limit(base);
    if mantissa >> F::SIGNIFICAND_SIZE != 0 {
        // Would require truncation of the mantissa.
        (F::ZERO, false)
    } else {
        let float: F = as_(mantissa);
        if exponent == 0 {
            // 0 exponent, same as value, exact representation.
            (float,  true)
        } else if exponent >= min_exp && exponent <= max_exp {
            // Value can be exactly represented, return the value.
            let float = unsafe { float.pow(base, exponent) };
            (float, true)
        } else {
            // Cannot be exactly represented, exponent multiplication
            // would require truncation.
            (F::ZERO, false)
        }
    }
}

// EXTENDED
// --------

// Moderate path for the parse algorithm.
// In this case, the mantissa can be represented by an integer,
// however, the exponent cannot be represented without truncating bytes.

// EXTENDED

/// Count the relative error in the extended-float precision.
struct Errors {
    // Upper bound for the error, in scale * ulp
    count: u32,
}

impl Errors {
    /// Error scale
    const ERROR_SCALE: u32 = 8;

    #[inline(always)]
    fn new(truncated: bool) -> Errors {
        Errors { count: Errors::trunction(truncated) }
    }

    #[inline(always)]
    fn trunction(truncated: bool) -> u32 {
        truncated as u32 * Self::halfscale()
    }

    #[inline(always)]
    fn scale() -> u32 {
        Self::ERROR_SCALE
    }

    #[inline(always)]
    fn halfscale() -> u32 {
        Self::scale() / 2
    }

    /// Determine if the number of errors is tolerable for float precision.
    #[inline]
    fn is_accurate<F: Float>(self, fp: &FloatType) -> bool {
        // Determine if extended-precision float is a good approximation.
        // If the error has affected too many units, the float will be
        // inaccurate.
        let bias = -(F::EXPONENT_BIAS - F::SIGNIFICAND_SIZE);
        let denormal_exp = bias - 63;
        // This is always a valid u32, since (denormal_exp - fp.exp)
        // will always be positive and the significand size is {23, 52}.
        let extrabits: u32 = as_(match fp.exp < denormal_exp {
            true  => 63 - F::SIGNIFICAND_SIZE + 1 + denormal_exp - fp.exp,
            false => 63 - F::SIGNIFICAND_SIZE,
        });

        // Do a signed comparison, which will always be valid.
        let halfway: i64 = as_(1u64 << (extrabits - 1));
        let extra: i64 = as_(fp.frac & ((1u64 << extrabits) - 1));
        let errors: i64 = as_(self.count);
        let cmp1 = (halfway - errors) < extra;
        let cmp2 = extra < (halfway + errors);

        // If both comparisons are true, we have significant rounding error,
        // and the value cannot be exactly represented. Otherwise, the
        // representation is valid.
        !(cmp1 && cmp2)
    }
}

/// Multiply the floating-point by the exponent.
///
/// Multiply by pre-calculated powers of the base, modify the extended-
/// float, and return if new value and if the value can be represented
/// accurately.
#[inline]
unsafe fn multiply_exponent_extended<F>(mut fp: FloatType, base: u32, exponent: i32, truncated: bool)
    -> (FloatType, bool)
    where F: Float
{
    let powers = cached::get_powers(base);
    let exponent = exponent + powers.bias;
    let small_index = exponent % powers.step;
    let large_index = exponent / powers.step;
    if exponent < 0 {
        // Guaranteed underflow (assign 0).
        (FloatType { frac: 0, exp: 0 }, true)
    } else if large_index as usize >= powers.large.len() {
        // Overflow (assign infinity)
        (FloatType { frac: 1 << 63, exp: 0x7FF }, true)
    } else {
        // Within the valid exponent range, multiply by the large and small
        // exponents and return the resulting value.

        // Track errors to as a factor of unit in last-precision.
        let mut errors = Errors::new(truncated);

        // Multiply by the small power.
        // Check if we can directly multiply by an integer, if not,
        // use extended-precision multiplication.
        match fp.frac.overflowing_mul(powers.get_small_int(small_index as usize)) {
            // Overflow, multiplication unsuccessful, go slow path.
            (_, true)     => {
                fp.normalize();
                fp.imul(powers.get_small(small_index as usize));
                errors.count += Errors::halfscale();
            },
            // No overflow, multiplication successful.
            (frac, false) => {
                fp.frac = frac;
                fp.normalize();
            },
        }

        // Multiply by the large power
        fp.imul(powers.get_large(large_index as usize));
        errors.count += (errors.count > 0) as u32;
        errors.count += Errors::halfscale();

        // Normalize the floating point (and the errors).
        let shift = fp.normalize();
        errors.count <<= shift;

        (fp, errors.is_accurate::<F>(&fp))
    }
}

/// Create a precise native float using an intermediate extended-precision float.
///
/// Return the float approximation and if the value can be accurately
/// represented with mantissa bits of precision.
#[inline]
pub(super) fn to_extended<F>(mantissa: u64, base: u32, exponent: i32, truncated: bool)
    -> (F, bool)
    where F: FloatRounding
{
    let fp = FloatType { frac: mantissa, exp: 0 };
    let (fp, valid) = unsafe { multiply_exponent_extended::<F>(fp, base, exponent, truncated) };
    if valid {
        (fp.as_native::<F>(), true)
    } else {
        (F::ZERO, false)
    }
}

// BIGNUM

// Super slow path...
// TODO(ahuszagh) Implement...

// ATOF/ATOD

/// Parse native float from string.
///
/// The float string must be non-special, non-zero, and positive.
#[inline]
unsafe extern "C" fn to_native<F>(base: u32, first: *const u8, last: *const u8)
    -> (F, *const u8)
    where F: FloatRounding + StablePower
{
    let (mantissa, exponent, p, truncated) = parse_float(base, first, last);
    let pow2_exp = pow2_exponent(base);

    if mantissa == 0 {
        // Literal 0, return early.
        // Value cannot be truncated, since we discard leading 0s whenever we
        // have mantissa == 0.
        return (F::ZERO, p);
    } else if pow2_exp != 0 {
        // We have a power of 2, can get an exact value even if the mantissa
        // was truncated, since we introduce no rounding error during
        // multiplication.
        let (float, valid) = pow2_to_exact::<F>(mantissa, base, pow2_exp, exponent);
        if valid {
            return (float, p);
        }
    } else if !truncated {
        // Try last fast path to exact, no mantissa truncation
        let (float, valid) = to_exact::<F>(mantissa, base, exponent);
        if valid {
            return (float, p);
        }
    }

    // Moderate path (use an extended 80-bit representation).
    let (float, valid) = to_extended::<F>(mantissa, base, exponent, truncated);
    if valid {
        return (float, p);
    }

    // Slow path (use a decimal representation).
    unreachable!()
}

/// Parse 32-bit float from string.
#[inline]
#[allow(dead_code)]     //TODO(ahuszagh) remove
pub(crate) unsafe extern "C" fn atof(base: u32, first: *const u8, last: *const u8)
    -> (f32, *const u8)
{
    to_native::<f32>(base, first, last)
}

/// Parse 64-bit float from string.
#[inline]
#[allow(dead_code)]     //TODO(ahuszagh) remove
pub(crate) unsafe extern "C" fn atod(base: u32, first: *const u8, last: *const u8)
    -> (f64, *const u8)
{
    to_native::<f64>(base, first, last)
}

// TESTS
// -----

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn check_parse_exponent(base: u32, s: &str, tup: (i32, usize)) {
        let first = s.as_ptr();
        let last = first.add(s.len());
        let (v, p) = parse_exponent(base, first, last);
        assert_eq!(v, tup.0);
        assert_eq!(distance(first, p), tup.1);
    }

    #[test]
    fn parse_exponent_test() {
        unsafe {
            // empty
            check_parse_exponent(10, "", (0, 0));

            // lowercase
            check_parse_exponent(10, "e20", (20, 3));
            check_parse_exponent(10, "e+20", (20, 4));
            check_parse_exponent(10, "e-20", (-20, 4));

            // uppercase
            check_parse_exponent(10, "E20", (20, 3));
            check_parse_exponent(10, "E+20", (20, 4));
            check_parse_exponent(10, "E-20", (-20, 4));

            // >= base15
            check_parse_exponent(15, "^20", (30, 3));
            check_parse_exponent(15, "^+20", (30, 4));
            check_parse_exponent(15, "^-20", (-30, 4));

            // overflow
            check_parse_exponent(10, "e10000000000", (i32::max_value(), 12));
            check_parse_exponent(10, "e+10000000000", (i32::max_value(), 13));
            check_parse_exponent(10, "e-10000000000", (-i32::max_value(), 13));

            // trailing
            check_parse_exponent(10, "e20 ", (20, 3));
            check_parse_exponent(10, "e+20 ", (20, 4));
        }
    }

    unsafe fn check_parse_mantissa(base: u32, s: &str, tup: (u64, i32, usize, bool))
    {
        let first = s.as_ptr();
        let last = first.add(s.len());
        let (v, d, p, t) = parse_mantissa(base, first, last);
        assert_eq!(v, tup.0);
        assert_eq!(d, tup.1);
        assert_eq!(distance(first, p), tup.2);
        assert_eq!(t, tup.3);
    }

    #[test]
    fn parse_mantissa_test() {
        unsafe {
            check_parse_mantissa(10, "1.2345", (12345, 4, 6, false));
            check_parse_mantissa(10, "12.345", (12345, 3, 6, false));
            check_parse_mantissa(10, "12345.6789", (123456789, 4, 10, false));
            check_parse_mantissa(10, "1.2345e10", (12345, 4, 6, false));
            check_parse_mantissa(10, "0.0000000000000000001", (1, 19, 21, false));
            check_parse_mantissa(10, "0.00000000000000000000000000001", (1, 29, 31, false));
            check_parse_mantissa(10, "100000000000000000000", (10000000000000000000, -1, 21, true));
        }
    }

    #[test]
    fn normalize_exponent_test() {
        assert_eq!(normalize_exponent(10, 5), 5);
        assert_eq!(normalize_exponent(0, 5), -5);
        assert_eq!(normalize_exponent(i32::max_value(), 5), i32::max_value());
        assert_eq!(normalize_exponent(i32::min_value(), 5), i32::min_value());
    }

    #[test]
    fn normalize_mantissa_test() {
        assert_eq!(normalize_mantissa(100, 10, 0), (1, 2));
        assert_eq!(normalize_mantissa(101, 10, 0), (101, 0));
        assert_eq!(normalize_mantissa(110, 10, 0), (11, 1));
    }

    unsafe fn check_parse_float(base: u32, s: &str, tup: (u64, i32, usize, bool))
    {
        let first = s.as_ptr();
        let last = first.add(s.len());
        let (v, e, p, t) = parse_float(base, first, last);
        assert_eq!(v, tup.0);
        assert_eq!(e, tup.1);
        assert_eq!(distance(first, p), tup.2);
        assert_eq!(t, tup.3);
    }

    #[test]
    fn parse_float_test() {
        unsafe {
            check_parse_float(10, "1.2345", (12345, -4, 6, false));
            check_parse_float(10, "12.345", (12345, -3, 6, false));
            check_parse_float(10, "12345.6789", (123456789, -4, 10, false));
            check_parse_float(10, "1.2345e10", (12345, 6, 9, false));
            check_parse_float(10, "100000000000000000000", (1, 20, 21, true));
        }
    }

    const POW2: [u32; 5] = [2, 4, 8, 16, 32];
    const BASEN: [u32; 30] = [
        3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21,
        22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 33, 34, 35, 36
    ];

    #[test]
    fn pow2_to_float_exact_test() {
        // Everything is valid.
        let mantissa = 1 << 63;
        for base in POW2.iter().cloned() {
            let (min_exp, max_exp) = f32::exponent_limit(base);
            let pow2_exp = pow2_exponent(base);
            for exp in min_exp-20..max_exp+30 {
                let (_, valid) = pow2_to_exact::<f32>(mantissa, base, pow2_exp, exp);
                assert!(valid, "should be valid {:?}.", (mantissa, base, exp));
            }
        }
    }

    #[test]
    fn pow2_to_double_exact_test() {
        // Everything is valid.
        let mantissa = 1 << 63;
        for base in POW2.iter().cloned() {
            let (min_exp, max_exp) = f64::exponent_limit(base);
            let pow2_exp = pow2_exponent(base);
            for exp in min_exp-20..max_exp+30 {
                let (_, valid) = pow2_to_exact::<f64>(mantissa, base, pow2_exp, exp);
                assert!(valid, "should be valid {:?}.", (mantissa, base, exp));
            }
        }
    }

    #[test]
    fn to_float_exact_test() {
        // valid
        let mantissa = 1 << (f32::SIGNIFICAND_SIZE - 1);
        for base in BASEN.iter().cloned() {
            let (min_exp, max_exp) = f32::exponent_limit(base);
            for exp in min_exp..max_exp+1 {
                let (_, valid) = to_exact::<f32>(mantissa, base, exp);
                assert!(valid, "should be valid {:?}.", (mantissa, base, exp));
            }
        }

        // invalid mantissa
        let (_, valid) = to_exact::<f32>(1<<f32::SIGNIFICAND_SIZE, 3, 0);
        assert!(!valid, "invalid mantissa");

        // invalid exponents
        for base in BASEN.iter().cloned() {
            let (min_exp, max_exp) = f32::exponent_limit(base);
            let (_, valid) = to_exact::<f32>(mantissa, base, min_exp-1);
            assert!(!valid, "exponent under min_exp");

            let (_, valid) = to_exact::<f32>(mantissa, base, max_exp+1);
            assert!(!valid, "exponent above max_exp");
        }
    }

    #[test]
    fn to_double_exact_test() {
        // valid
        let mantissa = 1 << (f64::SIGNIFICAND_SIZE - 1);
        for base in BASEN.iter().cloned() {
            let (min_exp, max_exp) = f64::exponent_limit(base);
            for exp in min_exp..max_exp+1 {
                let (_, valid) = to_exact::<f64>(mantissa, base, exp);
                assert!(valid, "should be valid {:?}.", (mantissa, base, exp));
            }
        }

        // invalid mantissa
        let (_, valid) = to_exact::<f64>(1<<f64::SIGNIFICAND_SIZE, 3, 0);
        assert!(!valid, "invalid mantissa");

        // invalid exponents
        for base in BASEN.iter().cloned() {
            let (min_exp, max_exp) = f64::exponent_limit(base);
            let (_, valid) = to_exact::<f64>(mantissa, base, min_exp-1);
            assert!(!valid, "exponent under min_exp");

            let (_, valid) = to_exact::<f64>(mantissa, base, max_exp+1);
            assert!(!valid, "exponent above max_exp");
        }
    }

    #[test]
    fn to_float_extended_test() {
        // valid (overflowing small mult)
        let mantissa = 1 << 63;
        let (f, valid) = to_extended::<f32>(mantissa, 3, 1, false);
        assert_eq!(f, 2.7670116e+19);
        assert!(valid, "exponent should be valid");
    }

    #[test]
    fn to_double_extended_test() {
        // valid (overflowing small mult)
        let mantissa = 1 << 63;
        let (f, valid) = to_extended::<f64>(mantissa, 3, 1, false);
        assert_eq!(f, 2.7670116110564327e+19);
        assert!(valid, "exponent should be valid");

        // valid (ends of the earth, salting the earth)
        let (f, valid) = to_extended::<f64>(mantissa, 3, -695, true);
        assert_eq!(f, 2.32069302345e-313);
        assert!(valid, "exponent should be valid");

        // invalid ("268A6.177777778", base 15)
        let mantissa = 4746067219335938;
        let (_, valid) = to_extended::<f64>(mantissa, 15, -9, false);
        assert!(!valid, "exponent should be invalid");
    }

    // TODO(ahuszagh) slow path
    // TODO(ahuszagh) atof, atod
    // Check both known fast and slow paths.
}
