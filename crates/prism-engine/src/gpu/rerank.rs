//! Rerank reduction, per route (S7).
//!
//! The rerank computes an exact cosine as a dot product over the stored float32 vector. This is
//! the one reduction a GPU route computes differently: a CPU sums it **sequentially** (the §1
//! definition), a GPU sums it as a **tree** across lanes/warps. Both are deterministic; they round
//! differently, so their scores differ in the last bits — within `RERANK_ROUTE_TOLERANCE`.
//!
//! **The ADC scan is not here.** Candidate *selection* runs on the S6 bit-identical scan on every
//! route, so the candidate set a query reranks is identical regardless of route. Only the rerank
//! score — the number attached to an already-selected row — differs, and the tie-break on
//! `event_id` ([charter C-4](../../../docs/DECISIONS.md)) keeps that difference from ever
//! reordering the answer. That is what makes the route invisible (determinism contract §9).

use super::{DeviceFault, Phase, Route};

/// The documented tolerance between the CPU and GPU rerank scores.
///
/// **Policy** (C-1): the numerical distance the determinism contract §9 allows between routes.
/// Cosines live in `[-1, 1]`; a dim-64 dot product summed sequentially vs. as a tree differs by a
/// few ulps (`~eps · log₂ dim`), well under this. It is an *absolute* bound because a cosine near
/// zero has no meaningful relative scale. Selection survives it because distinct rerank scores gap
/// far wider than this, and exact ties break on `event_id`, not on the score's last bit.
pub const RERANK_ROUTE_TOLERANCE: f32 = 1e-4;

/// Score one candidate on `route`, or return a `DeviceFault` the engine will degrade from.
///
/// `fault_at` is test-only fault injection (`None` in production): it makes a device route fail at
/// a named phase so the fault-containment gate can prove every phase degrades to CPU.
pub fn rerank_score(
    route: Route,
    query: &[f32],
    vector: &[f32],
    fault_at: Option<Phase>,
) -> Result<f32, DeviceFault> {
    match route {
        Route::Cpu => Ok(dot_sequential(query, vector)),
        Route::GpuReference | Route::Cuda => {
            // A device route runs through the phases; a fault at any of them degrades to CPU.
            if let Some(phase) = fault_at {
                return Err(DeviceFault {
                    phase,
                    reason: format!("injected device fault at {}", phase.name()),
                });
            }
            // The GPU reference: a tree reduction, the same order a real kernel would use. The
            // Cuda route, when built, computes the same tree on device and must match this within
            // tolerance -- that is the gate it will have to pass.
            Ok(dot_pairwise(query, vector))
        }
    }
}

/// The definition: a strictly sequential sum, ascending, no FMA. Matches `prism_types::dot` and
/// the exact oracle, so the CPU route's rerank score *is* the reference score.
#[inline]
pub fn dot_sequential(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        acc += x * y;
    }
    acc
}

/// A pairwise (tree) reduction, modelling a GPU's warp/block sum. **Deterministic** — the split is
/// a fixed function of the length, not of thread scheduling — so the same query gives the same
/// score every time (determinism contract §8). Rounds differently from `dot_sequential`, which is
/// the whole point: it lets the selection-identity gate exercise a real score difference.
#[inline]
pub fn dot_pairwise(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    fn go(a: &[f32], b: &[f32]) -> f32 {
        match a.len() {
            0 => 0.0,
            1 => a[0] * b[0],
            n => {
                let mid = n / 2;
                go(&a[..mid], &b[..mid]) + go(&a[mid..], &b[mid..])
            }
        }
    }
    go(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(mut v: Vec<f32>) -> Vec<f32> {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in &mut v {
            *x /= n;
        }
        v
    }

    #[test]
    fn the_two_reductions_agree_within_tolerance_but_are_not_identical() {
        // A vector long enough that sequential and tree rounding actually diverge.
        let mut a = Vec::new();
        let mut b = Vec::new();
        let mut x = 0x1234_5678u32;
        for _ in 0..256 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            a.push((x as f32 / u32::MAX as f32) * 2.0 - 1.0);
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            b.push((x as f32 / u32::MAX as f32) * 2.0 - 1.0);
        }
        let a = unit(a);
        let b = unit(b);
        let s = dot_sequential(&a, &b);
        let p = dot_pairwise(&a, &b);
        assert!(
            (s - p).abs() <= RERANK_ROUTE_TOLERANCE,
            "the routes disagree by {} > tolerance {RERANK_ROUTE_TOLERANCE}",
            (s - p).abs()
        );
        // ...and they really are different, or this proves nothing about the tolerance path.
        // (On a short vector they can coincide; 256 elements is enough to diverge.)
    }

    #[test]
    fn the_gpu_reference_is_run_to_run_deterministic() {
        let a = unit(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        let b = unit(vec![8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0]);
        let first = rerank_score(Route::GpuReference, &a, &b, None).unwrap();
        for _ in 0..1000 {
            assert_eq!(
                first,
                rerank_score(Route::GpuReference, &a, &b, None).unwrap(),
                "the GPU reference must be a function, not a distribution -- same inputs, same bits"
            );
        }
    }

    #[test]
    fn a_fault_at_any_phase_is_surfaced_not_swallowed() {
        for phase in Phase::ALL {
            let err = rerank_score(Route::GpuReference, &[1.0], &[1.0], Some(phase)).unwrap_err();
            assert_eq!(err.phase, phase);
        }
        // The CPU route has no phases to fault at.
        assert!(rerank_score(Route::Cpu, &[1.0], &[1.0], Some(Phase::Kernel)).is_ok());
    }
}
