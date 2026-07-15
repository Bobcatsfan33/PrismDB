//! ADC scan kernels (S6) — one definition, several instruction sets, one answer.
//!
//! The scan computes, for every coded row in a range, the asymmetric distance
//!
//! ```text
//! adc(code) = Σ_{j=0}^{m-1} table[j·256 + code[j]]
//! ```
//!
//! summed strictly in ascending `j`, in binary32, with no FMA and no reordering
//! ([docs/DETERMINISM-CONTRACT.md](../../../docs/DETERMINISM-CONTRACT.md) §1.1). That definition
//! is chosen to be **free to vectorize losslessly**: the reduction is *per row*, so the
//! parallelism lives **across rows — one row per SIMD lane** — never across the `j` within a row.
//!
//! Each lane runs the identical `m`-step ascending chain on its own row. Lane-wise IEEE-754
//! addition is correctly rounded and identical to the scalar addition, so **every lane's result
//! is bit-identical to the scalar reference.** There is no horizontal reduction of a distance,
//! nothing is ever summed in tree order, and there is no epsilon. The gather that fetches a
//! lane's table value is an exact load of an exact f32; it changes which value is added, never
//! how it is added.
//!
//! That is why the strong form of the determinism gate is *achievable on every ISA* rather than
//! aspired to, and why the SIMD engine returns byte-identical answers to the scalar one — only
//! faster.

/// Sub-quantizer codeword count. A code byte indexes one of 256 codewords.
pub const KSUB: usize = 256;

/// Which instruction set a scan should use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Isa {
    /// The reference. It *is* the definition; every other kernel is measured against it.
    Scalar,
    /// x86-64 AVX2: 8 rows per iteration, hardware gather.
    Avx2,
    /// x86-64 AVX-512F: 16 rows per iteration. **Behind `experimental-avx512`** until CI can
    /// execute it (Intel SDE or a self-hosted runner). See the determinism contract §3.
    Avx512,
    /// aarch64 NEON: 4 rows per iteration, manual gather.
    Neon,
}

impl Isa {
    pub fn name(self) -> &'static str {
        match self {
            Isa::Scalar => "scalar",
            Isa::Avx2 => "avx2",
            Isa::Avx512 => "avx512",
            Isa::Neon => "neon",
        }
    }
}

use std::sync::atomic::{AtomicU8, Ordering};

/// A test-only ceiling on kernel selection.
///
/// The dispatcher never picks an ISA above this. It exists so the feature-masking gate can force
/// each fallback — mask AVX2 down to scalar, run the full determinism suite, prove the fallback
/// is not just present but *correct* (determinism contract §3). 0 means "no ceiling".
static ISA_CEILING: AtomicU8 = AtomicU8::new(0);

fn isa_rank(i: Isa) -> u8 {
    match i {
        Isa::Scalar => 1,
        Isa::Neon => 2,
        Isa::Avx2 => 3,
        Isa::Avx512 => 4,
    }
}

/// Cap kernel selection at `max`. For tests only; the ceiling is process-global.
pub fn set_isa_ceiling(max: Isa) {
    ISA_CEILING.store(isa_rank(max), Ordering::SeqCst);
}

pub fn clear_isa_ceiling() {
    ISA_CEILING.store(0, Ordering::SeqCst);
}

fn under_ceiling(i: Isa) -> bool {
    let c = ISA_CEILING.load(Ordering::SeqCst);
    c == 0 || isa_rank(i) <= c
}

/// Every ISA this build could use on this machine, best first. The determinism gate runs the
/// scan through each and asserts they agree.
pub fn available() -> Vec<Isa> {
    let mut v = vec![Isa::Scalar];
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is mandatory on aarch64 -- no runtime check needed, it is always present.
        v.push(Isa::Neon);
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            v.push(Isa::Avx2);
        }
        #[cfg(feature = "experimental-avx512")]
        if std::arch::is_x86_feature_detected!("avx512f") {
            v.push(Isa::Avx512);
        }
    }
    v.retain(|&i| under_ceiling(i));
    v
}

/// The best kernel the running CPU supports, under any test ceiling. Falls back to scalar, which
/// is always available.
pub fn best() -> Isa {
    available()
        .into_iter()
        .max_by_key(|&i| isa_rank(i))
        .unwrap_or(Isa::Scalar)
}

/// Compute `out[i] = adc(codes[i·m .. (i+1)·m])` for `i in 0..out.len()`, using `isa`.
///
/// `codes` is row-major (each row's `m` bytes contiguous), exactly as stored. `out` is a
/// caller-owned buffer, reused across ranges so the hot loop allocates nothing (determinism
/// contract §4).
pub fn adc_scan(isa: Isa, table: &[f32], m: usize, codes: &[u8], out: &mut [f32]) {
    let n = out.len();
    debug_assert_eq!(codes.len(), n * m);
    debug_assert!(table.len() >= m * KSUB);
    if n == 0 {
        return;
    }
    match isa {
        Isa::Scalar => adc_scan_scalar(table, m, codes, out),
        #[cfg(target_arch = "aarch64")]
        Isa::Neon => unsafe { adc_scan_neon(table, m, codes, out) },
        #[cfg(target_arch = "x86_64")]
        Isa::Avx2 => unsafe { adc_scan_avx2(table, m, codes, out) },
        #[cfg(all(target_arch = "x86_64", feature = "experimental-avx512"))]
        Isa::Avx512 => unsafe { adc_scan_avx512(table, m, codes, out) },
        // A kernel selected on the wrong architecture is a dispatch bug, not a runtime condition.
        // The scalar definition is always correct, so fall back to it rather than panic.
        #[allow(unreachable_patterns)]
        _ => adc_scan_scalar(table, m, codes, out),
    }
}

/// The reference. This function *is* the definition in [§1.1]; it does not merely implement it.
#[inline]
pub fn adc_scan_scalar(table: &[f32], m: usize, codes: &[u8], out: &mut [f32]) {
    for (i, slot) in out.iter_mut().enumerate() {
        let code = &codes[i * m..(i + 1) * m];
        let mut acc = 0.0f32;
        for (j, &c) in code.iter().enumerate() {
            // Ascending j, plain add, no FMA. The chain the whole contract is about.
            acc += table[j * KSUB + c as usize];
        }
        *slot = acc;
    }
}

// --- aarch64 NEON: 4 rows per iteration ------------------------------------------------------

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn adc_scan_neon(table: &[f32], m: usize, codes: &[u8], out: &mut [f32]) {
    use std::arch::aarch64::*;

    let n = out.len();
    let groups = n / 4;

    for g in 0..groups {
        let i0 = g * 4;
        // Four independent accumulators, one per row. Each will run the identical ascending-j
        // chain as the scalar reference for its row, so each lane is bit-identical.
        let mut acc = vdupq_n_f32(0.0);
        for j in 0..m {
            // NEON has no gather, so the four table values are fetched with scalar loads. This is
            // index math, not distance math: the *values* are the exact f32s the scalar path
            // would add, and the add below is the only floating-point operation.
            let base = j * KSUB;
            let lanes = [
                table[base + codes[(i0) * m + j] as usize],
                table[base + codes[(i0 + 1) * m + j] as usize],
                table[base + codes[(i0 + 2) * m + j] as usize],
                table[base + codes[(i0 + 3) * m + j] as usize],
            ];
            let t = vld1q_f32(lanes.as_ptr());
            acc = vaddq_f32(acc, t); // lane-wise IEEE add == scalar add, per lane
        }
        vst1q_f32(out.as_mut_ptr().add(i0), acc);
    }

    // The tail runs the scalar definition, which is bit-identical by construction.
    let done = groups * 4;
    if done < n {
        adc_scan_scalar(table, m, &codes[done * m..], &mut out[done..]);
    }
}

// --- x86-64 AVX2: 8 rows per iteration, hardware gather --------------------------------------

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn adc_scan_avx2(table: &[f32], m: usize, codes: &[u8], out: &mut [f32]) {
    use std::arch::x86_64::*;

    let n = out.len();
    let groups = n / 8;

    for g in 0..groups {
        let i0 = g * 8;
        let mut acc = _mm256_setzero_ps();
        for j in 0..m {
            let base = (j * KSUB) as i32;
            // Build the eight gather indices: table[j*256 + code[row][j]] for the 8 rows.
            let idx = _mm256_setr_epi32(
                base + codes[(i0) * m + j] as i32,
                base + codes[(i0 + 1) * m + j] as i32,
                base + codes[(i0 + 2) * m + j] as i32,
                base + codes[(i0 + 3) * m + j] as i32,
                base + codes[(i0 + 4) * m + j] as i32,
                base + codes[(i0 + 5) * m + j] as i32,
                base + codes[(i0 + 6) * m + j] as i32,
                base + codes[(i0 + 7) * m + j] as i32,
            );
            // Gather is an exact load of eight exact f32s. Scale 4 = sizeof(f32).
            let t = _mm256_i32gather_ps::<4>(table.as_ptr(), idx);
            acc = _mm256_add_ps(acc, t); // lane-wise IEEE add == scalar add, per lane
        }
        _mm256_storeu_ps(out.as_mut_ptr().add(i0), acc);
    }

    let done = groups * 8;
    if done < n {
        adc_scan_scalar(table, m, &codes[done * m..], &mut out[done..]);
    }
}

// --- x86-64 AVX-512F: 16 rows per iteration (experimental, gated) ----------------------------

#[cfg(all(target_arch = "x86_64", feature = "experimental-avx512"))]
#[target_feature(enable = "avx512f")]
unsafe fn adc_scan_avx512(table: &[f32], m: usize, codes: &[u8], out: &mut [f32]) {
    use std::arch::x86_64::*;

    let n = out.len();
    let groups = n / 16;

    for g in 0..groups {
        let i0 = g * 16;
        let mut acc = _mm512_setzero_ps();
        for j in 0..m {
            // Manual gather into an array, then a single vector load -- exactly the NEON approach
            // at 16 lanes. This deliberately avoids `_mm512_i32gather_ps`, whose intrinsic
            // signature has churned across std versions, in favour of `_mm512_loadu_ps`, which is
            // stable. The gathered values are the exact f32s the scalar path would add, and the
            // only floating-point op is the lane-wise add below, so the result stays bit-identical.
            let base = j * KSUB;
            let mut lanes = [0.0f32; 16];
            for (lane, slot) in lanes.iter_mut().enumerate() {
                *slot = table[base + codes[(i0 + lane) * m + j] as usize];
            }
            let t = _mm512_loadu_ps(lanes.as_ptr());
            acc = _mm512_add_ps(acc, t); // lane-wise IEEE add == scalar add, per lane
        }
        _mm512_storeu_ps(out.as_mut_ptr().add(i0), acc);
    }

    let done = groups * 16;
    if done < n {
        adc_scan_scalar(table, m, &codes[done * m..], &mut out[done..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic table and codes, then assert every available kernel is
    /// **bit-identical** to the scalar reference. Not epsilon-close -- identical bits.
    #[test]
    fn every_available_kernel_is_bit_identical_to_the_reference() {
        // A range of `m` values so the sweep exercises tails (n not divisible by any lane count)
        // and several sub-quantizer counts.
        for &m in &[4usize, 8, 16] {
            // A pseudo-random but deterministic table.
            let mut table = vec![0.0f32; m * KSUB];
            let mut x = 0x1234_5678u32;
            for slot in table.iter_mut() {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Values with real fractional parts, so a reordered sum would round differently.
                *slot = (x as f32 / u32::MAX as f32) * 3.0 - 1.5;
            }
            // Row counts chosen to leave awkward tails for every lane width (4, 8, 16).
            for &n in &[0usize, 1, 3, 7, 15, 17, 100, 133] {
                let mut codes = vec![0u8; n * m];
                for c in codes.iter_mut() {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *c = (x >> 16) as u8;
                }

                let mut want = vec![0.0f32; n];
                adc_scan_scalar(&table, m, &codes, &mut want);

                for isa in available() {
                    let mut got = vec![0.0f32; n];
                    adc_scan(isa, &table, m, &codes, &mut got);
                    // Compare BITS, not values: NaN and rounding are exact obligations here.
                    let wbits: Vec<u32> = want.iter().map(|v| v.to_bits()).collect();
                    let gbits: Vec<u32> = got.iter().map(|v| v.to_bits()).collect();
                    assert_eq!(
                        wbits,
                        gbits,
                        "kernel {} disagreed with the scalar reference at m={m}, n={n}. The \
                         determinism contract requires bit-identical results, and a one-ulp \
                         disagreement here becomes a different query answer.",
                        isa.name()
                    );
                }
            }
        }
    }

    #[test]
    fn the_ceiling_forces_the_fallback() {
        set_isa_ceiling(Isa::Scalar);
        assert_eq!(
            best(),
            Isa::Scalar,
            "a scalar ceiling must force the scalar kernel"
        );
        assert_eq!(available(), vec![Isa::Scalar]);
        clear_isa_ceiling();
    }
}
