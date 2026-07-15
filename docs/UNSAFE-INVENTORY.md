# The `unsafe` Inventory

**Status:** started in S6, where `unsafe` starts in earnest — SIMD intrinsics and memory-mapped I/O. Before S6 the codebase had none; the charter's "audit all unsafe" was vacuous, and now it is not.

Every `unsafe` block in the shipping crates is listed here with its safety argument and the test or fuzz target that covers it. **CI fails if an `unsafe` token exists in `crates/*/src` without a corresponding entry** (`scripts/check-unsafe-inventory.sh`, grep-level enforcement). The count below is asserted against the source, so a new `unsafe` block that is not documented here turns CI red.

The rule this enforces: an `unsafe` block is a place where the compiler stops checking and a human promise takes over. A promise nobody wrote down is a promise nobody can review.

**Total `unsafe` tokens (code, not comments) in `crates/*/src`: 11.**

---

## `crates/prism-quantizer/src/kernel.rs` — SIMD ADC scan (7)

The determinism contract ([docs/DETERMINISM-CONTRACT.md](DETERMINISM-CONTRACT.md) §1) requires every kernel to be **bit-identical** to the scalar reference. The design that guarantees this — vectorizing *across rows*, one row per lane, each lane running the identical ascending-`j` accumulation — is also what makes the unsafe small and local: the only floating-point operation is a lane-wise add, and the gather is an exact load.

| line | `unsafe` | safety argument | covered by |
|---|---|---|---|
| dispatch × 3 | calling `adc_scan_neon` / `adc_scan_avx2` / `adc_scan_avx512` | Each is reached only after `available()` confirmed the running CPU has the feature (`is_x86_feature_detected!` for x86; NEON is mandatory on aarch64), and only on its own `target_arch`. AVX-512 is additionally behind the `experimental-avx512` cfg. | `kernel::tests::every_available_kernel_is_bit_identical_to_the_reference`; the CPU-feature masking gate `determinism::masking_the_cpu_forces_the_fallback...` |
| `adc_scan_neon` | `#[target_feature(enable="neon")]` fn; `vld1q_f32` / `vaddq_f32` / `vst1q_f32` | NEON is always present on aarch64, so the target feature is satisfied unconditionally. All loads/stores are in-bounds: the group loop covers `n/4*4` rows and the scalar tail handles the rest; `out.add(i0)` writes 4 lanes at `i0 < groups*4 <= n`. The gathered indices `j*256 + code[..]` are `< m*256 <= table.len()` (a `u8` code is `< 256`). | bit-identity test above; run natively on Apple Silicon in CI |
| `adc_scan_avx2` | `#[target_feature(enable="avx2")]` fn; `_mm256_i32gather_ps` / `_mm256_add_ps` / `_mm256_storeu_ps` | Reached only when `is_x86_feature_detected!("avx2")`. The gather reads `table[idx]` with `idx = j*256 + code < m*256 <= table.len()`; the store writes 8 lanes at `i0 < groups*8 <= n` via the unaligned `storeu`. Tail handled scalar. | bit-identity test; the GitHub x86 runner executes it |
| `adc_scan_avx512` | `#[target_feature(enable="avx512f")]` fn; `_mm512_*` | Same argument at 16 lanes. **Behind `experimental-avx512`, off by default**, because no CI runner can execute it yet (determinism contract §3) — so it is written to the contract but not shipped enabled. | bit-identity test *when the feature is built*; not yet in CI |

**Why these are sound in one sentence:** every lane index into `table` is bounded by the `u8` code times the sub-quantizer count, every write is within the pre-sized `out`, and the target feature is confirmed before the block runs.

## `crates/prism-part/src/mmap.rs` — read-only memory mapping (5)

Parts are immutable, so a read-only mapping cannot race a writer; the only hazard is a **truncated file**, which `SIGBUS`es on access. The design makes that unreachable by bounds-checking every access against the file's real length ([the module docs](../crates/prism-part/src/mmap.rs)).

| line | `unsafe` | safety argument | covered by |
|---|---|---|---|
| `unsafe impl Send` / `Sync` (×2) | marking `Mmap` thread-safe | The mapping is read-only over an immutable file, owns its region exclusively, and has no interior mutability, so `&Mmap` is sound to share. | used across the engine's part readers under the concurrent-query tests |
| `Mmap::open` | `ffi::mmap(...)` | `fd` is a live descriptor borrowed from `file` for the call. `PROT_READ | MAP_PRIVATE` is a private read-only map; we never write through the pointer. `len` is the file's real length, so every in-range byte is backed. The result is checked against `MAP_FAILED`. Ownership passes to the returned `Mmap`, whose `Drop` unmaps exactly `len`. | `mmap::tests::*`; `fuzz::a_truncated_framed_column_under_mmap_names_itself_and_never_sigbuses` |
| `Mmap::slice` | `slice::from_raw_parts(ptr.add(offset), len)` | `offset..offset+len` is checked `<= self.len` immediately above, and `self.len` is the mapped (= file) length, so every byte is backed — an in-range access never touches an unbacked page, so it never `SIGBUS`es. The returned slice borrows `self`, so it cannot outlive the mapping. | the truncated-part fault test above (truncates at many lengths, asserts named errors, proves no `SIGBUS`) |
| `Mmap::drop` | `ffi::munmap(ptr, len)` | `ptr`/`len` are exactly what `mmap` returned; `Drop` runs once, so no double-unmap; unmapping a live read-only mapping is always sound. | exercised on every part read in the suite (leak/behaviour would surface under the full run) |

---

## The enforcement

`scripts/check-unsafe-inventory.sh` counts `unsafe` tokens in `crates/*/src` and compares against the number this document claims (the **Total** line above). A mismatch — a new `unsafe` block, or a removed one this file still lists — fails CI. It is deliberately crude: the point is that *adding* `unsafe` forces a diff to this file, where a reviewer sees the safety argument, not that a script understands the argument.

When you add or remove an `unsafe` block, update the table **and** the total, in the same change.

## S7 note — the device boundary adds no `unsafe` yet

S7 is "GPU-ready, GPU-off": there is no CUDA hardware and no GPU CI runner, so the real CUDA FFI is declared behind `#[cfg(feature = "cuda")]` (in `crates/prism-engine/src/gpu/mod.rs`) and **not compiled** — it adds zero `unsafe` to the shipping build, and the count stays 11. When a runner exists and the CUDA kernels land, every `cuLaunchKernel` / `cuMemAlloc` / `cuMemcpy` call site joins this inventory **with its error-recovery path** (directive 8): each maps a CUDA error to a `DeviceFault` the engine degrades from, never a panic. The kernels stay behind the one narrow `gpu` module — the `Isa` dispatch pattern — so a device pointer or a CUDA error never reaches query-planning code. If this inventory ever sprawls into the planner, the abstraction boundary is wrong.
