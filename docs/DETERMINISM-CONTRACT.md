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

---

# The device edition (S7)

**Status:** written in S7, before the first GPU kernel — and S7 ships with **no GPU kernel and no GPU in CI**, because this environment has neither CUDA hardware nor the cloud credentials to provision a runner. Per the architect's own fallback, **S7 is "GPU-ready, GPU-off": it does not claim the GPU gate.** What it builds is everything *device-agnostic* — the route abstraction, the fault-containment path, the fp16 accuracy contract, per-tenant device admission — all tested against a **CPU reference of the GPU route**, which is the definition the real CUDA kernel will one day have to prove itself equal to, exactly as the scalar ADC kernel is the definition every SIMD kernel proves itself equal to (§1).

The device edition adds two requirements, weaker than §2's strong form and deliberately so, because a GPU **cannot** be bit-identical to a CPU — FMA contraction, a different reduction order, and non-associative parallel sums are the default and often unavoidable in GPU arithmetic.

## 8. Run-to-run determinism on the same device

> Same query, same snapshot, **same answer, every time** — on a given device.

A GPU kernel that sums with atomics accumulates in **nondeterministic order**, so two runs of the same query return two different scores and, at a tie boundary, two different answers. That is forbidden. Each kernel launch config pins a **fixed reduction order** — a fixed grid/block shape and a deterministic tree reduction, never `atomicAdd` into a shared accumulator — so the device is a *function*, not a distribution. The reference route (the CPU definition) is trivially deterministic; the gate is on the real kernel, and it is checked by running the same query many times and asserting one answer.

## 9. Selection-identity between the CPU and GPU routes

> Scores may differ within a **documented tolerance**; the returned **event IDs and their order may not.**

This is the device analogue of §2's weak form, and it is the property the whole sprint turns on. The GPU route computes the same distances and cosines as the CPU route, in a different arithmetic, so its scores differ in the last few bits. But **selection** — which rows survive the scan, which order they rerank into — must be **identical**, because selection is what a bounded top-k *decides* and it is what a user sees. Two routes that select differently are two databases wearing one API.

Selection-identity rests on [charter C-4](DECISIONS.md), already law: distance ties break on `event_id`, never on a score's last bit. So a score difference below the tolerance cannot reorder the answer, because the tie-break is on the id. The gate checks it on the golden, layout-variant, and boundary-tie corpora: the CPU route and the GPU-reference route return **byte-identical event-id lists**, and the scores agree within the declared tolerance.

**Why this is what pagination requires.** A cursor pins a snapshot and a position in the total order ([query contract](QUERY-CONTRACT.md) §2). If routing the query to the GPU on page 2 changed the order, the cursor from page 1 — computed on the CPU — would point into a different sequence, and the client would see duplicates or gaps. **The route must be invisible to the answer.** So the gate paginates a result set while **forcing the route to flip between pages**, and asserts the pages still tile the snapshot exactly: no duplicate, no gap. A cursor must survive a route change, or routing is not allowed to exist.

## 10. fp16 rerank is a negotiated accuracy contract, not a kernel detail

Storing rerank vectors in fp16 halves the exact-tier storage bill — the biggest open cost question in the system — at the price of approximation. That is a [D-003](DECISIONS.md) event, a change in *what a stored byte means*, not a kernel optimization, and it is handled the way D-003 built the format to handle it: **the part's rerank-tier descriptor declares its `encoding_id` and its `accuracy_contract_id`, and the reader dispatches on the pair.**

**fp32-exact remains the only default.** fp16 ships behind an explicit `accuracy_contract_id` whose tolerance and whose **selection-stability evidence** are committed as a receipt — the first *negotiated* accuracy contract in the system, and the precedent for every lossy encoding after it. The rule the precedent sets: a lossy encoding may enter the format only with (a) its contract text stating the tolerance, (b) a committed receipt proving selection stability on the golden corpus at that tolerance, and (c) the unknown-encoding fixture updated in the same change, so a build that does *not* implement the contract still refuses the part loudly rather than guessing.

## 11. A device fault degrades to CPU; it never fails a query

A CUDA error — out of memory, a launch failure, the device lost mid-query — **degrades to the CPU path with a logged event.** Never a failed query, never a wrong answer, never corrupted engine state. The GPU is an accelerator, not a dependency; a query that could be answered on the CPU is always answered. Fault-injection tests cover device loss at **every phase boundary** — upload, kernel, selection, download — and assert the query still returns the CPU answer.

And **device-memory admission is per tenant, and lands with the kernels, not after.** A device OOM caused by tenant A's oversized query must not fail tenant B's — the same starvation-isolation property the ingest path already enforces, now on the device. A tenant's device footprint is admitted against a per-tenant budget before a byte is uploaded.

## 12. The crossover is a cost model, and its numbers are the worst device's

Routing CPU-vs-GPU is a **measured** decision, not a heuristic: the thresholds (bytes to transfer, candidate width, selectivity, queue depth) are C-1 tuned constants with C-3 policy bounds, measured **end to end** — transfer, launch, selection, rerank — never kernel time, because a kernel-only number is a roofline and the charter forbids rooflines. The published matrix **includes where GPU loses** — a selective query with a small candidate set pays the upload and launch cost for a scan too short to amortize it, and loses; the honest matrix says so. And the quoted number is the **worst device's**, exactly as §7's is the worst ISA's.

**In S7 these thresholds cannot be derived**, because deriving them requires measuring a GPU, and there is none. So the cost-model *mechanism* is built and the thresholds are constants marked **device-conditional and un-derived**, with the sweep filed to run the moment a runner exists. Charter **C-6** makes that re-derivation a standing obligation, not a hope.

# The semantic-aggregate edition (S9)

**Status:** written in S9, before the clustering code. S9 adds a *randomized* algorithm — k-means over PQ codes — to an engine whose every prior answer was a deterministic function of the data. A randomized algorithm is where determinism contracts usually die: a different seed, a different iteration order, a different reduction, and the clusters move. Everything below is how S9 refuses that. It is the **randomized-algorithm edition of [charter C-5](DECISIONS.md)** ([C-7](DECISIONS.md)): *the answer is a function of the data — not of a wall-clock seed, and not of the order the rows happened to be stored in.*

## 13. `semantic_cluster` is a deterministic function of `(logical row set, k, generation)`

> Cluster the **same rows** with the **same k** in the **same generation**, and get the **same clusters, the same exemplars, and the same per-cluster aggregates** — byte-identical — every time, on every layout, under every plan and route.

Three things make a k-means result move, and each is nailed down:

- **The seed.** The PRNG is **seeded from content**, never from a clock or a counter: `seed = SHA-256(sorted event_ids ‖ k ‖ generation)`. Two stores holding the same logical rows seed identically regardless of when or where they run; a store that gains or loses a row reseeds, because the row set *is* the input. There is no `Math.random()` anywhere near this — a wall-clock seed would make the answer a function of *when you asked*, which is exactly what C-5 forbids, one axis over.
- **The initialization.** k-means++ over the decoded PQ centroids, using that seeded PRNG, with candidates considered in **logical (`event_id`) order** so the D²-weighted draw is reproducible. Lucky-init dependence is the bug [D-036](DECISIONS.md) already killed once for the codebook; the same discipline applies here.
- **The order rows are consumed.** Mini-batches are drawn in **logical order** — sorted by `event_id` — **never in scan order.** Scan order is a fact about physical layout (which part, which block, which route fetched it); making the clusters depend on it would be [C-4](DECISIONS.md)/[C-5](DECISIONS.md) violated at the batch level. A row's contribution to a centroid update is a function of its identity and its position in the *sorted* set, not of where it lives.

Float reductions inside the centroid update obey §1's rule — a centroid coordinate is an ascending sum in a fixed (logical) order, no FMA — so the model itself is bit-reproducible, not merely close. The gate asserts identical clusters/exemplars/aggregates across the layout-variant fixtures **and** across forced plan-flips and route-flips (the S8/S7 controls turned on the aggregate): the physical strategy that fetched the rows is invisible to the clustering, as it is to every other answer.

## 14. The merge of distributed partial states is canonical-order, and that is a property, not a hope

The aggregate is built to scale out ([PRISM.md](PRISM.md) S12) before it scales out: each partition produces a **partial state** — per cluster, a count, the scalar-aggregate sums (each accumulated in that partition's logical order), and a bounded exemplar-candidate selection — and the global answer is the **merge** of those partials.

> Partials merge in **canonical order: sorted partition id.** The merge sorts its inputs before it combines them, so the *physical* order partials arrive in cannot change the result.

This matters because a float sum is not associative: merging partition sums in arrival order would make `avg(cost)` a function of which partition's network packet landed first. So the merge is defined as a fixed fold over partition id ascending, and the **partial-state property test asserts merge-order invariance** directly — it presents the same partials in scrambled orders and requires byte-identical output, not merely *correct* output. Correctness a lenient test would accept; invariance is the actual contract, because it is what makes a distributed answer equal to a single-node one. (Distributed *model fit* — computing the centroids themselves from partials — is **not** in S9; S9 fits the model single-node and only the aggregate is partitioned-and-merged. The distributed fit is filed, not faked.)

## 15. Exemplars are a C-4 bounded selection on the exact score

An exemplar is the most-central *actual event* of a cluster, and choosing it is a **bounded selection**, so [charter C-4](DECISIONS.md) governs it exactly as it governs the top-k: **most-central by exact score, ties broken on `event_id` ascending, never on physical position.** And the score is the **exact** rerank distance to the centroid, never the PQ distance alone — a cluster is *labeled* by its exemplar, and a mislabeled cluster is a wrong answer a user reads and believes, worse than a slow one. The PQ code decides cluster membership (it is what the aggregate is cheap over); the exact vector decides the one event we put a name on. The gate proves the tie-break on the boundary-tie corpus, where two events are equidistant from a centroid and only the `event_id` rule keeps the exemplar stable across layouts.
