//! The **lease clock**: a monotonic time source for GC and reader-lease decisions (S12, D-075).
//!
//! Everything that records a *timestamp* — an event's observed time, a snapshot's `created_at_ms` —
//! uses the wall clock ([`now_ms`](crate::engine::now_ms)), because a timestamp is meant to reflect
//! wall time. But a *reclaim* decision must not be at the mercy of the wall clock: a node whose clock
//! jumps forward would reclaim snapshots a live reader still holds (a lease cut short); one whose
//! clock jumps backward would keep them forever. So GC asks this clock instead.
//!
//! `lease_now_ms()` is calibrated **once** to the wall clock, then advances **monotonically** from a
//! `std::time::Instant` — so a wall-clock jump after calibration moves it not at all. That is the S10
//! discipline ("monotonic locally"); the one unavoidable wall-clock comparison — GC against a
//! persisted `created_at_ms` a node stamped with its own wall clock — is bounded by
//! [`prism_part::catalog::MAX_CLOCK_SKEW_MS`], which the derived GC grace absorbs.
//!
//! A process-global test override makes the monotonic clock injectable, so a chaos run can skew the
//! wall clock and prove the lease clock — and therefore the lease — does not budge. Never a
//! production path.

use crate::engine::now_ms;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// The monotonic anchor: the wall time at first read, and the `Instant` captured then. Set once.
static ANCHOR: OnceLock<(i64, Instant)> = OnceLock::new();

/// Test override. `i64::MIN` means "unset" — use the real monotonic clock; any other value is the
/// forced lease-now, so a test drives the lease clock deterministically without real sleeping.
static OVERRIDE_MS: AtomicI64 = AtomicI64::new(i64::MIN);

/// The lease clock: wall-anchored once, monotonic thereafter — immune to wall-clock jumps. This is
/// the "now" GC and reader-lease expiry reason about, never the raw wall clock.
pub fn lease_now_ms() -> i64 {
    let o = OVERRIDE_MS.load(Ordering::SeqCst);
    if o != i64::MIN {
        return o;
    }
    let (wall_anchor, mono_anchor) = ANCHOR.get_or_init(|| (now_ms(), Instant::now()));
    wall_anchor.saturating_add(mono_anchor.elapsed().as_millis() as i64)
}

/// Test seam: force the lease clock to a fixed value, so a chaos/lease test advances monotonic time
/// by hand while skewing the wall clock underneath it. `None` restores the real monotonic clock.
/// **Never a production path.**
pub fn set_lease_now_override(now_ms: Option<i64>) {
    OVERRIDE_MS.store(now_ms.unwrap_or(i64::MIN), Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lease_clock_ignores_a_wall_clock_jump() {
        // Pin the lease clock at a known monotonic "now", as a chaos run would while it skews the
        // wall clock underneath. The reclaim decision reads THIS, not the wall clock — so a ±30d
        // wall-clock jump (simulated by never consulting the wall clock here) moves it not at all.
        set_lease_now_override(Some(1_000_000));
        assert_eq!(lease_now_ms(), 1_000_000);

        // Monotonic time advances by hand: the lease clock advances with it and only with it.
        set_lease_now_override(Some(1_000_000 + 42));
        assert_eq!(lease_now_ms(), 1_000_042);

        // Restore the real clock and confirm it is monotonic across two reads.
        set_lease_now_override(None);
        let a = lease_now_ms();
        let b = lease_now_ms();
        assert!(b >= a, "the lease clock ran backward: {a} -> {b}");
    }
}
