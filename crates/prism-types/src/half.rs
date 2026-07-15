//! IEEE-754 binary16 (half precision) conversion (S7).
//!
//! Hand-rolled, not pulled from the `half` crate, because the charter keeps the dependency tree
//! at serde alone ([D-002](../../../docs/DECISIONS.md)). Half precision is a well-specified format
//! — 1 sign bit, 5 exponent, 10 mantissa — and the conversion is a fixed bit manipulation, so
//! writing it in-tree costs a hundred lines and owes nobody a version bump.
//!
//! Storing a rerank vector in fp16 halves the exact-tier storage bill at the price of a bounded
//! approximation. That is a [D-003](../../../docs/DECISIONS.md) event — a change in *what a stored
//! byte means* — governed by a negotiated accuracy contract, not a kernel detail. This module is
//! the arithmetic under that contract; the contract itself lives on the rerank-tier descriptor.

/// Round an `f32` to the nearest `f16`, ties to even, and return its 16 bits.
///
/// Round-to-nearest-even is IEEE's default and the only choice that does not bias a sum of many
/// roundings, which matters because a rerank score *is* a sum of many rounded products.
pub fn f32_to_f16_bits(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32; // biased f32 exponent
    let mant = bits & 0x007f_ffff;

    if exp == 0xff {
        // Inf / NaN. Preserve NaN-ness (non-zero mantissa) so a NaN never becomes an Inf.
        let m = if mant != 0 { 0x0200 } else { 0 };
        return sign | 0x7c00 | m;
    }

    // Rebias to f16 (bias 15 vs f32's 127).
    let mut e = exp - 127 + 15;

    if e >= 0x1f {
        // Overflow -> Inf.
        return sign | 0x7c00;
    }
    if e <= 0 {
        // Subnormal or zero. Shift the implicit-1 mantissa into place and round.
        if e < -10 {
            return sign; // rounds to zero
        }
        let m = mant | 0x0080_0000; // restore the implicit leading 1
        let shift = (14 - e) as u32; // 14..24
        let half = m >> shift;
        // Round to nearest even on the bits shifted out.
        let rem_mask = (1u32 << shift) - 1;
        let rem = m & rem_mask;
        let halfway = 1u32 << (shift - 1);
        let round_up = rem > halfway || (rem == halfway && (half & 1) == 1);
        return sign | (half + u32::from(round_up)) as u16;
    }

    // Normal number. f16 mantissa is the top 10 bits of the f32 mantissa; round the rest.
    let m10 = (mant >> 13) as u16;
    let rem = mant & 0x1fff;
    let halfway = 0x1000;
    let mut out_mant = m10;
    if rem > halfway || (rem == halfway && (m10 & 1) == 1) {
        out_mant += 1;
        if out_mant == 0x0400 {
            // Mantissa overflow carries into the exponent.
            out_mant = 0;
            e += 1;
            if e >= 0x1f {
                return sign | 0x7c00; // rounded up into Inf
            }
        }
    }
    sign | ((e as u16) << 10) | out_mant
}

/// Widen an `f16` (given by its bits) back to `f32`. Exact — every `f16` is representable in
/// `f32`, so this direction loses nothing.
pub fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x03ff) as u32;

    let bits = if exp == 0 {
        if mant == 0 {
            sign // +/- zero
        } else {
            // Subnormal f16 -> normalized f32.
            let mut e = -1i32;
            let mut m = mant;
            while m & 0x0400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x03ff;
            let f32_exp = (e + 1 - 15 + 127) as u32;
            sign | (f32_exp << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        // Inf / NaN.
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        let f32_exp = exp + 127 - 15;
        sign | (f32_exp << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// `f32 -> f16 -> f32`: the value as it would round-trip through fp16 storage. This *is* the lossy
/// value a rerank against an fp16 part sees.
pub fn round_trip_f16(x: f32) -> f32 {
    f16_bits_to_f32(f32_to_f16_bits(x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_small_integers_survive_the_round_trip() {
        for i in -1000..=1000 {
            let x = i as f32;
            assert_eq!(round_trip_f16(x), x, "fp16 should represent {x} exactly");
        }
    }

    #[test]
    fn zero_and_signs_are_preserved() {
        assert_eq!(round_trip_f16(0.0).to_bits(), 0.0f32.to_bits());
        assert_eq!(round_trip_f16(-0.0).to_bits(), (-0.0f32).to_bits());
        assert!(round_trip_f16(1.0) > 0.0);
        assert!(round_trip_f16(-1.0) < 0.0);
    }

    #[test]
    fn rounding_error_is_bounded_by_half_precision() {
        // For values in a cosine's range, the relative error of one round-trip is <= 2^-11.
        let mut x = 0x1234_5678u32;
        for _ in 0..100_000 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let v = (x as f32 / u32::MAX as f32) * 2.0 - 1.0; // in [-1, 1]
            let r = round_trip_f16(v);
            if v.abs() > 1e-3 {
                let rel = ((r - v) / v).abs();
                assert!(
                    rel <= 2f32.powi(-10),
                    "fp16 round-trip of {v} had relative error {rel}, above the half-precision bound"
                );
            }
        }
    }

    #[test]
    fn nan_stays_nan_and_inf_stays_inf() {
        assert!(round_trip_f16(f32::NAN).is_nan());
        assert!(round_trip_f16(f32::INFINITY).is_infinite());
        // A value too large for f16 overflows to infinity, by design.
        assert!(round_trip_f16(1e30).is_infinite());
    }
}
