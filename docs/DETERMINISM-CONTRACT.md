# The Determinism Contract

**Status:** written in S6, *before* the first SIMD kernel — because a SIMD kernel that is "fast and approximately equal" is a kernel that returns a different answer on a different machine, and an analytical database that answers a question differently depending on which CPU ran the query is not one database, it is a family of subtly disagreeing ones.

This is [D-033](DECISIONS.md) again — *the answer is a function of the data, not of where the data is stored* — extended to its float edition: **the answer is a function of the data, not of which instruction set computed it.**

Where this contract and a kernel disagree, **the contract is right and the kernel is a bug.**

---

## 1. The two floating-point reductions, defined

A query's answer flows through exactly two floating-point reductions, and this document *defines* both. A definition, not a description: the scalar code does not set the standard by accident of being written first. The standard is stated here, and **every implementation — the scalar reference included — is measured against it.**

### 1.1 The ADC distance (the scan)

For a coded row with sub-quantizer bytes `code[0..m]` and a query's per-query lookup table `table`:

```
adc(code) = Σ_{j=0}^{m-1} table[j·256 + code[j]]
```

**summed strictly in ascending `j`, in IEEE-754 binary32, with no fused multiply-add and no reordering.** The sum is a chain of `m` plain additions, `((((t₀ + t₁) + t₂) + …) + t_{m-1})`.

This definition is chosen so that it is **free to vectorize losslessly.** The reduction is *per row*, and a scan computes it for millions of rows — so the parallelism lives **across rows, one row per SIMD lane**, never across the `j` within a row. Each lane runs the identical `m`-step ascending chain on its own row. Lane-wise IEEE-754 addition is correctly rounded and identical to the scalar addition, so **every lane's result is bit-identical to the scalar reference.** No horizontal reduction of a distance ever happens; nothing is ever summed in tree order; there is no `epsilon`.

That is the whole trick, and it is why the strong form (§2) is achievable on every ISA rather than aspired to.

### 1.2 The exact score (the rerank)

The rerank computes a cosine as a dot product over `dim` stored float32 values. It is **defined to be the scalar sequential dot product**, and it is **computed only by the scalar reference on every ISA.**

This is a deliberate asymmetry, and it is the honest one. A dot product's natural SIMD form accumulates into lanes and then *horizontally* reduces — which is a tree sum, a different order, a different rounding, a different number. Vectorizing it losslessly would mean pinning a lane-count-independent reduction tree and forcing every ISA to emulate it, for a reduction that runs over the **rerank width** (tens of rows), not the scan (millions). The scan is where SIMD earns its keep; the rerank is not. So the rerank stays scalar, is bit-identical across ISAs by being *the same code*, and the per-ISA baseline (§7) reports its time as ISA-invariant — which is a true fact about this engine, not a gap in it.

A consequence worth stating: because both reductions are bit-identical across ISAs, **a whole query is bit-identical across ISAs.** The SIMD engine and the scalar engine compute the *same numbers*; SIMD only computes the scan's numbers *faster*. This is what makes the C-1 receipts survive the sprint unchanged ([§5](#5-constants-are-engine-conditional)).

## 2. The gate: identical answers, not epsilon-close scores

> Identical event IDs, identical ordering — **the same answer**, byte for byte — across scalar, AVX2, AVX-512 and NEON, on the layout-variant golden fixtures.

Not "scores within tolerance". The gate compares **answers**: the ordered list of event IDs a query returns, and the scores attached to them, exactly. It runs on the same frozen corpus the [C-4 layout gate](DECISIONS.md) uses, materialized the same several ways, so a kernel that disagrees with the reference disagrees visibly and a merge cannot hide it.

The strong form is the standard, and this engine meets it: bit-identical values everywhere.

### The weak-form fallback, and why it exists

Some future kernel — the 4-bit SIMD-shuffle PQ scan, which quantizes the lookup table to `uint8` and uses saturating byte adds — **cannot** be bit-identical, by construction: its arithmetic is not IEEE-754 at all. That kernel is not in S6, but the contract anticipates it, because the alternative is to forbid it forever or to let it in quietly.

If exact score equality is genuinely unattainable on some ISA or kernel, the fallback is narrow and enforced:

> Scores may differ, but **candidate *selection* may not.** The set of rows that survive the scan into the rerank, and the final ordering after rerank, must be identical to the reference.

Selection is what a bounded top-k *decides*, and it is where a score difference turns into a different answer. Proving selection-invariance requires the [C-4 tie-break](DECISIONS.md) (already law: distance ties break on `event_id`, never on position) **plus** a boundary-tie stress corpus — rows placed at deliberately equal and near-equal distances, where a one-`ulp` disagreement would flip which row is kept. A weak-form kernel must pass that corpus.

**The strong form is preferred, always.** The weak form is accepted only with a decision logged in [DECISIONS.md](DECISIONS.md) naming the kernel, the ISA, and the reason equality was unattainable. S6 ships entirely strong-form; there is no weak-form kernel yet, and this section is the gate the first one will have to pass.

## 3. An ISA that CI cannot execute does not ship enabled

A kernel nobody can run is a kernel nobody can prove. So the rule is mechanical:

| ISA | how CI runs it | default state |
|---|---|---|
| scalar | everywhere | always on (the reference) |
| AVX2 | GitHub x86-64 runner | on when the CPU has it |
| NEON | GitHub ARM runner / Apple Silicon | on when the CPU has it (baseline on aarch64) |
| **AVX-512** | **needs Intel SDE or a self-hosted runner — neither is in CI yet** | **behind `experimental-avx512`, off by default** |

AVX-512 is written to the same contract and gated behind a Cargo feature. It does not compile into a default build and cannot be selected at runtime in one. When CI can execute it — via Intel SDE emulation or a self-hosted runner — it graduates to the table's normal rule. Until then, shipping it enabled would mean shipping a path whose determinism no test has ever checked, which is the one thing this document exists to prevent.

**Runtime feature detection is itself a gate.** The dispatcher picks the best kernel the *running* CPU supports and falls back to scalar. A test masks the CPU features to force each fallback and runs the full determinism suite under it, so "the fallback works" is a checked fact, not a hope.

## 4. The hot loop allocates nothing

Asserted, not aspired. A counting allocator wraps the test harness, and the block scan and the bounded top-k perform **zero heap allocations** across a full golden run, after their working buffers are sized once.

This is why the top-k holds `(part, row)` indices rather than owned event IDs: the tie-break borrows the event ID out of the already-loaded scalar column, so a row entering the top-k costs no allocation. A distance buffer is sized once to the largest range and reused. The per-row hot path — the thing that runs millions of times — touches no allocator at all.

## 5. Constants are engine-conditional

SIMD changes the **cost model** under which `BLOCK_SIZE = 4 KiB` and the width receipts were derived. So the C-1 sweeps are re-run against the SIMD engine at sprint end, on the same corpora against the same floors, and the receipts either **reconfirm** or **re-derive** — either way the committed evidence describes the engine that actually ships ([charter C-1](DECISIONS.md)).

Because §1 makes the SIMD engine bit-identical to the scalar one, every *recall*-derived constant (`nprobe`, the widths) is expected to reconfirm exactly — the answers did not move, only their speed. A constant derived from *bytes read* (`BLOCK_SIZE`) is likewise unmoved, because SIMD changes how bytes are added, not how many are fetched. A reconfirmation is not a formality here; it is the determinism contract's prediction, checked.

## 6. `unsafe` is inventoried

S6 is where `unsafe` starts in earnest — SIMD intrinsics and memory-mapped I/O. Every `unsafe` block is listed in [UNSAFE-INVENTORY.md](UNSAFE-INVENTORY.md) with its safety argument and the test or fuzz target that covers it, and **CI fails if an `unsafe` block exists without an inventory entry.**

The mmap path is read-only over immutable parts — the easy case — with one sharp edge: a **truncated file under mmap SIGBUSes** on access rather than returning an error. The [S1 truncation discipline](DECISIONS.md) (a truncated part names its column, block and byte range) must survive the change from `read()` to mmap. It does, and it does so *by construction*: the map's length is the file's real length, every block range is checked against it **before** the byte is touched, and a range past the end is the same named `Corrupt` error it always was — the SIGBUS is never reached because the bounds check fires first. A fault test truncates a mapped part and asserts the named error.

## 7. Per-architecture numbers, end to end

The baseline report ([`baselines.json`](../baselines.json)) grows an **ISA dimension**. Every number is measured **end to end** — transfer, scan, selection, rerank — never kernel-only, because a kernel-only number is a roofline wearing a stopwatch, and the charter forbids rooflines dressed as measurements.

And the p50 quoted anywhere — a README, a release note, this repo's own claims — is **the worst supported ISA's**, never the best's. A number that is only true on your fastest machine is a number that is false on the machine your customer runs.
