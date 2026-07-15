//! **The hot loop allocates nothing — asserted, not aspired (S6, determinism contract §4).**
//!
//! > *"counting allocator in the test harness, zero allocations inside the block scan and top-k
//! > under a full golden run."*
//!
//! A counting allocator wraps the system allocator *for this test binary only* (a
//! `#[global_allocator]` in an integration test does not touch the shipped `prism` binary). It
//! counts every allocation while armed. The two components the contract names — the block scan
//! (`kernel::adc_scan`) and the bounded top-k (`topk::TopK`) — run under it over a full golden
//! run's worth of rows, and the count must be **zero** after their buffers are sized once.
//!
//! This is why the scan writes distances into a reused buffer and the top-k holds `(part, row)`
//! indices instead of owned event ids: a row flowing through the hot path touches no allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    // realloc counts too: growing a Vec is exactly the allocation the hot loop must not do.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// Tests in one binary share this process's allocator and run on parallel threads, so a second
// test allocating while a first is ARMED would be miscounted as the first's. One lock, held for
// the whole body of every test here, makes the measurement single-threaded and honest.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` under the armed allocator and return how many allocations it made.
fn count<R>(f: impl FnOnce() -> R) -> (R, usize) {
    ALLOCS.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    let r = f();
    ARMED.store(false, Ordering::Relaxed);
    (r, ALLOCS.load(Ordering::Relaxed))
}

use prism_engine::topk::{Candidate, TopK};
use prism_quantizer::kernel::{self, Isa, KSUB};

fn table(m: usize) -> Vec<f32> {
    let mut t = vec![0.0f32; m * KSUB];
    let mut x = 0x9e37_79b9u32;
    for slot in t.iter_mut() {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *slot = (x as f32 / u32::MAX as f32) * 2.0 - 1.0;
    }
    t
}

fn codes(n: usize, m: usize) -> Vec<u8> {
    let mut c = vec![0u8; n * m];
    let mut x = 0x1234_5678u32;
    for b in c.iter_mut() {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *b = (x >> 20) as u8;
    }
    c
}

/// The block scan, over a large range, through every available kernel: **zero allocations** once
/// the distance buffer is sized.
#[test]
fn the_block_scan_allocates_nothing() {
    let _serial = SERIAL.lock().unwrap();
    let m = 8;
    let n = 20_000;
    let t = table(m);
    let cs = codes(n, m);
    let mut dists = vec![0.0f32; n]; // sized once, outside the measured region

    for isa in kernel::available() {
        // Warm: touch the code paths once before measuring, so first-call lazies do not count.
        kernel::adc_scan(isa, &t, m, &cs, &mut dists);

        let (_, allocs) = count(|| {
            // A full golden run scans on the order of this many rows; do it several times.
            for _ in 0..8 {
                kernel::adc_scan(isa, &t, m, &cs, &mut dists);
            }
        });
        assert_eq!(
            allocs,
            0,
            "kernel {} allocated {allocs} times inside the block scan. The scan is the \
             millions-of-times hot path; it must touch the allocator zero times.",
            isa.name()
        );
    }
}

/// The bounded top-k, offered far more rows than it keeps: **zero allocations** once its heap is
/// sized to `cap`.
#[test]
fn the_bounded_top_k_allocates_nothing() {
    let _serial = SERIAL.lock().unwrap();
    // Stable owned ids the borrow closure can point at. Built BEFORE arming.
    let ids: Vec<String> = (0..50_000).map(|i| format!("e{i:08}")).collect();
    let id_of = |_p: u32, row: u32| -> &str { ids[row as usize].as_str() };

    // Distances chosen so the heap churns: many rows are nearer than the current worst, forcing
    // real sift work (not just early rejection). A mix of ties exercises the id tie-break too.
    let dists: Vec<f32> = (0..50_000u32)
        .map(|i| ((i * 2_654_435_761) % 997) as f32)
        .collect();

    let cap = 200;
    // Warm one construction so any lazy init is not measured; the real measured TopK is built
    // OUTSIDE the counted region, because `new` legitimately allocates its buffer once.
    let mut topk = TopK::new(cap, &id_of);

    let (_, allocs) = count(|| {
        for (row, &d) in dists.iter().enumerate() {
            topk.offer(Candidate {
                dist: d,
                part: 0,
                row: row as u32,
            });
        }
        topk.len()
    });

    assert_eq!(
        allocs, 0,
        "the bounded top-k allocated {allocs} times while being offered 50,000 rows. It keeps \
         {cap}; the tie-break borrows event ids out of the resident scalar column rather than \
         owning copies, so a row entering the top-k must cost no allocation. This is exactly the \
         property the String-per-candidate design used to violate."
    );
}

/// Sanity: the counter actually counts. A test that cannot observe an allocation cannot prove
/// their absence.
#[test]
fn the_counter_is_not_asleep() {
    let _serial = SERIAL.lock().unwrap();
    let (_, allocs) = count(|| {
        let v: Vec<u8> = Vec::with_capacity(4096);
        v.len()
    });
    assert!(
        allocs >= 1,
        "the counting allocator did not see an obvious allocation"
    );
}

/// A scalar-only ceiling still allocates nothing in the scan.
#[test]
fn the_scalar_fallback_also_allocates_nothing() {
    let _serial = SERIAL.lock().unwrap();
    kernel::set_isa_ceiling(Isa::Scalar);
    let m = 8;
    let n = 5_000;
    let t = table(m);
    let cs = codes(n, m);
    let mut dists = vec![0.0f32; n];
    kernel::adc_scan(Isa::Scalar, &t, m, &cs, &mut dists);
    let (_, allocs) = count(|| kernel::adc_scan(Isa::Scalar, &t, m, &cs, &mut dists));
    kernel::clear_isa_ceiling();
    assert_eq!(allocs, 0);
}
