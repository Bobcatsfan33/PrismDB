//! Vector boundary rules (Part III §10).
//!
//! Vectors are normalized at ingest, so dot product is cosine and L2 ordering
//! agrees with cosine ordering. Everything downstream — coarse assignment, PQ
//! training, ADC scan, exact rerank — may assume unit norm. That assumption is
//! enforced *here*, at the boundary, and nowhere else.

use crate::error::{PrismError, Result};

/// Reject NaN/inf/zero-norm, then normalize in place.
pub fn validate_and_normalize(v: &mut [f32]) -> Result<()> {
    if v.is_empty() {
        return Err(PrismError::Invalid("embedding has zero dimensions".into()));
    }
    let mut sum_sq = 0.0f64;
    for (i, x) in v.iter().enumerate() {
        if !x.is_finite() {
            return Err(PrismError::Invalid(format!(
                "embedding component {i} is not finite: {x}"
            )));
        }
        sum_sq += (*x as f64) * (*x as f64);
    }
    let norm = sum_sq.sqrt();
    if !norm.is_finite() || norm < 1e-12 {
        return Err(PrismError::Invalid(format!(
            "embedding has zero or degenerate norm: {norm}"
        )));
    }
    for x in v.iter_mut() {
        *x = (*x as f64 / norm) as f32;
    }
    Ok(())
}

/// Squared L2 distance. On unit vectors this is `2 - 2·cos`, so ascending L2²
/// is exactly descending cosine — we rank on this everywhere and convert to a
/// cosine score only at the surface.
#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = 0.0f32;
    for i in 0..a.len() {
        let d = a[i] - b[i];
        acc += d * d;
    }
    acc
}

#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = 0.0f32;
    for i in 0..a.len() {
        acc += a[i] * b[i];
    }
    acc
}

/// Cosine similarity for unit vectors, clamped to the valid range so float
/// error can never produce a score outside [-1, 1].
#[inline]
pub fn cosine_from_l2_sq(l2_sq: f32) -> f32 {
    (1.0 - l2_sq / 2.0).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_unit_norm() {
        let mut v = vec![3.0, 4.0];
        validate_and_normalize(&mut v).unwrap();
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn rejects_nan_inf_and_zero_norm() {
        assert!(validate_and_normalize(&mut [1.0, f32::NAN]).is_err());
        assert!(validate_and_normalize(&mut [f32::INFINITY, 1.0]).is_err());
        assert!(validate_and_normalize(&mut [0.0, 0.0]).is_err());
        assert!(validate_and_normalize(&mut []).is_err());
    }

    #[test]
    fn l2_ordering_matches_cosine_ordering_on_unit_vectors() {
        let q = vec![1.0f32, 0.0];
        let mut near = vec![0.9f32, 0.1];
        let mut far = vec![-1.0f32, 0.2];
        validate_and_normalize(&mut near).unwrap();
        validate_and_normalize(&mut far).unwrap();
        assert!(l2_sq(&q, &near) < l2_sq(&q, &far));
        assert!(dot(&q, &near) > dot(&q, &far));
        // and the two agree numerically
        assert!((cosine_from_l2_sq(l2_sq(&q, &near)) - dot(&q, &near)).abs() < 1e-5);
    }
}
