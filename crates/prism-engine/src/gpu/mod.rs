//! The device boundary (S7) — one narrow module, per the `Isa` dispatch pattern.
//!
//! **S7 ships "GPU-ready, GPU-off".** This environment has no CUDA hardware and no credentials to
//! provision a GPU CI runner, so — per the architect's own fallback — S7 does *not* claim the GPU
//! gate. What it builds is everything **device-agnostic**: the route abstraction, the
//! fault-containment path, per-tenant device admission, and the selection-identity property, all
//! tested against a **CPU reference of the GPU route**.
//!
//! That reference is not a pretend GPU. It is the *definition* the real CUDA kernel will have to
//! prove itself equal to — exactly as the scalar ADC kernel is the definition every SIMD kernel
//! proves itself equal to ([docs/DETERMINISM-CONTRACT.md](../../../docs/DETERMINISM-CONTRACT.md)
//! §1). The one thing it models faithfully is the thing that *matters* for correctness: a GPU
//! reduces sums in a **different order** than a CPU (tree/pairwise, not sequential), so its scores
//! differ in the last bits. The reference reduces the same way, so the selection-identity gate
//! (§9) exercises a real score difference and proves the answer survives it.
//!
//! The real CUDA kernels live behind the `cuda` feature, off by default and not compiled in CI
//! (there is no device to run them). Their FFI boundary is declared here and nowhere else — the
//! whole point of a narrow module is that a device fault, a device pointer, or a CUDA error never
//! leaks into query-planning code (directive 8).

pub mod admission;
pub mod rerank;

pub use admission::{DeviceAdmission, DEVICE_MEMORY_BUDGET_BYTES};
pub use rerank::{rerank_score, RERANK_ROUTE_TOLERANCE};

use serde::{Deserialize, Serialize};

/// Which substrate a query's rerank runs on.
///
/// Generalizes S6's `Isa`: the GPU is "another substrate" the planner may route to. The route is
/// **invisible to the answer** — every route returns identical event ids in identical order
/// (selection-identity, determinism contract §9) — so the planner is free to choose on cost alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The CPU rerank: sequential reduction, the definition every other route matches.
    Cpu,
    /// The **CPU reference of the GPU route**: a deterministic tree reduction, modelling a GPU's
    /// different-but-fixed sum order. Its scores differ from `Cpu` within `RERANK_ROUTE_TOLERANCE`;
    /// its selection is identical. This is what ships and is tested in S7.
    GpuReference,
    /// The real CUDA rerank. **Off by default, behind the `cuda` feature, not in CI.** Selected
    /// only when a device is present *and* a build enabled it; until a GPU CI runner can prove it
    /// against `GpuReference`, it does not ship enabled (the AVX-512 rule, device edition).
    Cuda,
}

impl Route {
    pub fn name(self) -> &'static str {
        match self {
            Route::Cpu => "cpu",
            Route::GpuReference => "gpu-reference",
            Route::Cuda => "cuda",
        }
    }

    /// Is this a device route (subject to faults, admission, and degradation)?
    pub fn is_device(self) -> bool {
        matches!(self, Route::GpuReference | Route::Cuda)
    }
}

// --- the crossover cost model (S7, C-1 tuned, device-conditional / un-derived) ---------------
//
// Routing CPU-vs-GPU is a MEASURED decision (determinism contract §12), and these are the
// thresholds. **They cannot be derived in S7** -- deriving them requires measuring a GPU, and
// there is none -- so they are placeholders marked device-conditional and un-derived, and the
// engine ships with the GPU OFF, so they never actually route anything. The cost-model MECHANISM
// is real and tested; the VALUES wait for a runner (charter C-6, issue filed).

/// A device route is only worth its upload+launch cost above this many candidate rows. Below it, a
/// selective query with a small candidate set pays the transfer for a scan too short to amortize
/// it, and **loses** -- the honest matrix says so (§12). **Un-derived**: placeholder until a GPU
/// exists to measure the crossover.
pub const GPU_MIN_CANDIDATES: usize = usize::MAX; // MAX = "never route to GPU" while GPU is off

/// Whether a device is present *and* enabled in this build. Always false in S7: the `cuda` feature
/// is off and there is no device, so this is "GPU-off" made mechanical.
pub fn gpu_available() -> bool {
    cfg!(feature = "cuda") && false // no runtime device yet; the runner is not stood up
}

/// The route a query's rerank should run on, and why. The cost model in one place.
#[derive(Clone, Debug)]
pub struct RoutePlan {
    pub route: Route,
    pub reason: String,
}

/// Plan the route. With the GPU off (S7) this is always CPU; the crossover logic is present and
/// exercised by tests via a forced route, so when a device lands only `gpu_available` and the
/// thresholds change, not this shape.
pub fn plan_route(candidates: usize, forced: Option<Route>) -> RoutePlan {
    if let Some(r) = forced {
        return RoutePlan {
            route: r,
            reason: "forced by the caller (test / advanced)".into(),
        };
    }
    // `GPU_MIN_CANDIDATES` is usize::MAX while the GPU is off, so this comparison is intentionally
    // always false -- the placeholder threshold means "never route to GPU" until it is derived.
    #[allow(clippy::absurd_extreme_comparisons)]
    if gpu_available() && candidates >= GPU_MIN_CANDIDATES {
        return RoutePlan {
            route: Route::Cuda,
            reason: format!("{candidates} candidates >= GPU_MIN_CANDIDATES"),
        };
    }
    RoutePlan {
        route: Route::Cpu,
        reason: if gpu_available() {
            format!("{candidates} candidates < GPU_MIN_CANDIDATES; CPU wins")
        } else {
            "GPU is off (no device / feature disabled)".into()
        },
    }
}

// --- test-only device-fault injection --------------------------------------------------------
//
// A global, like S6's ISA ceiling, so the fault-containment gate can make a device route fail at a
// chosen phase and prove the engine degrades to CPU. `None` in production.

use std::sync::atomic::{AtomicU8, Ordering};

static FAULT_PHASE: AtomicU8 = AtomicU8::new(0); // 0 = none
static FORCED_ROUTE: AtomicU8 = AtomicU8::new(0); // 0 = none (cost model decides)

/// Force a route globally (test only), the way S6 forces an ISA ceiling. The route-flip
/// pagination gate flips this between pages to prove a cursor survives a route change.
pub fn set_forced_route(route: Option<Route>) {
    FORCED_ROUTE.store(
        match route {
            None => 0,
            Some(Route::Cpu) => 1,
            Some(Route::GpuReference) => 2,
            Some(Route::Cuda) => 3,
        },
        Ordering::SeqCst,
    );
}

pub fn forced_route_override() -> Option<Route> {
    match FORCED_ROUTE.load(Ordering::SeqCst) {
        1 => Some(Route::Cpu),
        2 => Some(Route::GpuReference),
        3 => Some(Route::Cuda),
        _ => None,
    }
}

/// Inject a device fault at `phase` (test only). Every device route will fail there until cleared.
pub fn set_fault(phase: Option<Phase>) {
    FAULT_PHASE.store(
        match phase {
            None => 0,
            Some(Phase::Upload) => 1,
            Some(Phase::Kernel) => 2,
            Some(Phase::Selection) => 3,
            Some(Phase::Download) => 4,
        },
        Ordering::SeqCst,
    );
}

pub fn injected_fault() -> Option<Phase> {
    match FAULT_PHASE.load(Ordering::SeqCst) {
        1 => Some(Phase::Upload),
        2 => Some(Phase::Kernel),
        3 => Some(Phase::Selection),
        4 => Some(Phase::Download),
        _ => None,
    }
}

/// A phase at which a device operation can fail. Fault injection walks these to prove every one
/// degrades to CPU rather than failing the query (determinism contract §11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Copying query + candidate vectors to the device.
    Upload,
    /// The rerank kernel launch.
    Kernel,
    /// Reading scores back and forming the selection.
    Selection,
    /// Copying results back to the host.
    Download,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Upload => "upload",
            Phase::Kernel => "kernel",
            Phase::Selection => "selection",
            Phase::Download => "download",
        }
    }

    pub const ALL: [Phase; 4] = [
        Phase::Upload,
        Phase::Kernel,
        Phase::Selection,
        Phase::Download,
    ];
}

/// A device fault. Carries the phase so the degradation log names what failed.
///
/// **This is the only error a device route may raise, and it is never propagated to the caller as
/// a query failure.** The engine catches it, logs the degradation, and re-runs on the CPU. A
/// device is an accelerator, not a dependency.
#[derive(Clone, Debug)]
pub struct DeviceFault {
    pub phase: Phase,
    pub reason: String,
}

/// What the engine records when a device route degrades to CPU. Observable so an operator sees the
/// GPU quietly stopped being used — a degradation that is silent is a GPU you are paying for and
/// not getting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Degradation {
    pub route: String,
    pub phase: String,
    pub reason: String,
    pub tenant: Option<String>,
}

// --- the real CUDA FFI boundary (off by default, not compiled in CI) -------------------------
//
// Declared here and NOWHERE else. When a GPU CI runner exists, these are the only `unsafe` call
// sites a device adds, each documented in docs/UNSAFE-INVENTORY.md with its error-recovery path.
// They are behind `#[cfg(feature = "cuda")]` so a default build neither compiles nor links CUDA.
#[cfg(feature = "cuda")]
pub mod cuda_ffi {
    //! The narrow CUDA FFI. Every call here maps a CUDA error to a `DeviceFault` (never a panic,
    //! never a wrong answer) and the engine degrades to CPU. NOT compiled in CI — no device.
    //!
    //! Intentionally left as the declared boundary the CUDA kernels will plug into: the runtime
    //! entry points (`cuMemAlloc`, `cuLaunchKernel`, `cuMemcpy*`) and the two PrismDB kernels
    //! (`prism_adc_scan`, `prism_rerank_dot`) compiled from `kernels/*.cu` by `build.rs` under the
    //! same feature. Until a runner can execute and prove them against `GpuReference`, this stays
    //! unbuilt — writing untestable FFI would be exactly the faked completeness the project
    //! refuses. See docs/PROGRESS.md, the S7 "GPU-off" record.
}

use std::sync::OnceLock;

/// The process-wide device admission. Device memory is a process resource, so admission is
/// process-global -- one tenant's reservations are visible to every query on the box, which is
/// exactly what makes the cross-tenant guarantee hold under concurrency.
pub fn admission() -> &'static DeviceAdmission {
    static ADMISSION: OnceLock<DeviceAdmission> = OnceLock::new();
    ADMISSION.get_or_init(DeviceAdmission::with_defaults)
}
