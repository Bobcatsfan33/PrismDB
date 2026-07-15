//! Per-tenant device-memory admission (S7, determinism contract §11).
//!
//! Device memory is a fixed, shared resource, and a GPU OOM is a **cross-tenant** failure by
//! default: tenant A uploads an oversized batch, the device runs out, and tenant B's concurrent
//! query — which would have fit — fails through no fault of its own. That is the same starvation
//! the ingest path already refuses (a loud tenant must not change a quiet tenant's latency), now
//! on the device, and it lands **with** the kernels, not after.
//!
//! The rule: a query's device footprint is admitted against a **per-tenant budget** before a byte
//! is uploaded. A tenant that would exceed its share is **degraded to CPU** — never failed, and
//! never allowed to consume another tenant's share. Admission is deterministic and needs no
//! device: it is arithmetic on declared footprints, so it is fully testable today.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Total device memory PrismDB will hand out to rerank work.
///
/// **Policy** (C-1): a bound on how much of a device rerank may hold at once, not a measured
/// optimum. Sized so the per-tenant share (this ÷ expected concurrent tenants) comfortably holds a
/// rerank batch (`rerank_width · dim · 4` bytes) with headroom. On a real device this is a
/// fraction of VRAM reserved for PrismDB; the value is **device-conditional** and re-derives when
/// a runner exists (charter C-6).
pub const DEVICE_MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// The share of the total budget a single tenant may hold at once.
///
/// **Policy** (C-1): the isolation guarantee. A tenant may never hold more than this fraction of
/// the device, so no tenant — however large its query — can starve the others. A quarter means up
/// to four tenants each get a full guaranteed share; a fifth is degraded to CPU rather than
/// permitted to eat into the four.
pub const PER_TENANT_SHARE: usize = 4;

/// Tracks live device reservations per tenant.
///
/// This is the whole isolation mechanism: a reservation is granted only if it fits the tenant's
/// remaining share *and* the global remaining budget, and it is released when the query's device
/// work finishes. Deterministic arithmetic — no device required to test that tenant A's demand
/// cannot fail tenant B.
pub struct DeviceAdmission {
    inner: Mutex<Inner>,
}

struct Inner {
    total_budget: usize,
    per_tenant_cap: usize,
    in_use: usize,
    by_tenant: BTreeMap<String, usize>,
}

/// A granted reservation. Releases itself on drop, so a query cannot leak device memory even if it
/// errors or degrades mid-flight.
pub struct Reservation<'a> {
    admission: &'a DeviceAdmission,
    tenant: String,
    bytes: usize,
    released: bool,
}

impl DeviceAdmission {
    pub fn new(total_budget: usize, per_tenant_cap: usize) -> Self {
        DeviceAdmission {
            inner: Mutex::new(Inner {
                total_budget,
                per_tenant_cap,
                in_use: 0,
                by_tenant: BTreeMap::new(),
            }),
        }
    }

    /// The default admission: the device budget, split so no tenant holds more than its share.
    pub fn with_defaults() -> Self {
        Self::new(
            DEVICE_MEMORY_BUDGET_BYTES,
            DEVICE_MEMORY_BUDGET_BYTES / PER_TENANT_SHARE,
        )
    }

    /// Try to reserve `bytes` of device memory for `tenant`.
    ///
    /// Returns `None` when the request would exceed the tenant's share or the global budget — the
    /// caller then **degrades this query to CPU**, which is always possible, rather than failing
    /// it. Crucially, a `None` for tenant A leaves tenant B's share **untouched**: A can never
    /// reserve into B's guaranteed portion, so A's demand cannot fail B.
    pub fn try_reserve(&self, tenant: &str, bytes: usize) -> Option<Reservation<'_>> {
        let mut g = self.inner.lock().unwrap();
        let held = g.by_tenant.get(tenant).copied().unwrap_or(0);
        if held + bytes > g.per_tenant_cap {
            return None; // would exceed this tenant's guaranteed share
        }
        if g.in_use + bytes > g.total_budget {
            return None; // device globally full
        }
        g.in_use += bytes;
        *g.by_tenant.entry(tenant.to_string()).or_insert(0) += bytes;
        Some(Reservation {
            admission: self,
            tenant: tenant.to_string(),
            bytes,
            released: false,
        })
    }

    fn release(&self, tenant: &str, bytes: usize) {
        let mut g = self.inner.lock().unwrap();
        g.in_use = g.in_use.saturating_sub(bytes);
        if let Some(t) = g.by_tenant.get_mut(tenant) {
            *t = t.saturating_sub(bytes);
            if *t == 0 {
                g.by_tenant.remove(tenant);
            }
        }
    }

    #[cfg(test)]
    fn in_use(&self) -> usize {
        self.inner.lock().unwrap().in_use
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.admission.release(&self.tenant, self.bytes);
            self.released = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_cannot_exceed_its_share() {
        // Budget 100, per-tenant cap 25. Reservations must be BOUND, not dropped -- a dropped
        // reservation releases immediately, which is itself the release-on-drop behaviour.
        let a = DeviceAdmission::new(100, 25);
        let _r1 = a.try_reserve("alpha", 20).expect("first fits");
        // alpha already holds 20; another 10 would exceed its 25 cap.
        assert!(a.try_reserve("alpha", 10).is_none());
        let _r2 = a
            .try_reserve("alpha", 5)
            .expect("5 more still fits the 25 cap");
    }

    #[test]
    fn tenant_a_cannot_fail_tenant_b() {
        // The property the whole module exists for. A budget where A greedily reserves its entire
        // share must leave B's share entirely available.
        let a = DeviceAdmission::new(100, 25);
        // alpha takes its full share; hold the reservation so it is not released.
        let _held = a.try_reserve("alpha", 25).expect("alpha's full share");
        // alpha is now capped; more is refused (degrade alpha to CPU) ...
        assert!(a.try_reserve("alpha", 1).is_none());
        // ... but bravo's share is untouched.
        assert!(
            a.try_reserve("bravo", 25).is_some(),
            "tenant alpha's demand ate into tenant bravo's guaranteed share -- the exact \
             cross-tenant failure this admission exists to prevent"
        );
    }

    #[test]
    fn releasing_frees_the_budget() {
        let a = DeviceAdmission::new(100, 100);
        {
            let _r = a.try_reserve("alpha", 60).unwrap();
            assert_eq!(a.in_use(), 60);
            // While held, a second 60 does not fit the global budget.
            assert!(a.try_reserve("bravo", 60).is_none());
        }
        // The reservation dropped; the device is free again.
        assert_eq!(a.in_use(), 0);
        assert!(a.try_reserve("bravo", 60).is_some());
    }

    #[test]
    fn the_default_share_holds_a_rerank_batch() {
        // A rerank batch is rerank_width * dim * 4 bytes. The per-tenant share must fit one with
        // headroom, or the device route would degrade every query and never be used.
        let a = DeviceAdmission::with_defaults();
        let batch = 50 * 64 * 4; // DEFAULT_RERANK * dim=64 * f32
        assert!(a.try_reserve("alpha", batch).is_some());
    }
}
