# Re-baselining obligation (S13 dir 2, opened at D-081)

**Record now, execute once — at dir-2 close, after the full family sweep.** Not per-constant churn.

## Why

Every *performance* receipt predating [D-081](../../docs/DECISIONS.md) was measured at
`DEFAULT_NPROBE = 6` — a probe count real 768d geometry does not support (at nprobe=6 the real-v1 p1
recall@10 is 0.400; the honest floor is nprobe=14, at 2.3× the scan). Its latency, scan-byte, and
throughput numbers therefore describe an operating point the engine no longer ships. They are **not
deleted** (paired-series discipline — a superseded number is data, not a lie), they are **tagged
`config_superseded`** and pointed here, and the headline baselines are **re-run at the re-derived
constants** before S16 benchmarks anything in public. S16 must inherit honest numbers, not flattering
ones.

The re-derived constants that move the operating point: `DEFAULT_NPROBE` 6→14, `ADAPTIVE_MARGIN`
0.05→0.02 (D-081), `THRESHOLD_OVERFETCH_MARGIN_EPSILON` 1e-6→0.30 (issue #8), **plus the remaining
dir-2 families** (widths, k-means restarts, block size, fp16 tolerance) — which is exactly why this is
one pass at the *end*, not per constant.

## Config-superseded receipts (tagged)

| Receipt | Measured at | Status |
|---|---|---|
| `block-size.json` | ~~`DEFAULT_NPROBE=6`, hash corpus~~ → **re-derived on real-v1** | **Resolved.** Re-derived on the real 768d corpus at restarts=8/nprobe=11 (D-083 pass): `DEFAULT_BLOCK_SIZE` **2048 → 16384**, because the 12×-wider exact-rerank column pushes smaller blocks past the directory-openability budget. The hash 64d sweep is retained as the paired series. No longer a pending baseline — it is a current constant receipt. |
| `s12-scaling.json` | explicit `nprobe=16`, hash corpus | **Still pending.** Used explicit probe counts, not the default, so it is not superseded *by the nprobe change alone* — but it predates D-081 and was measured on the hash corpus at a pre-real operating point. The scaling **lower bound** is re-run at the final constants (restarts=8 / nprobe=11) on real-v1 for an honest S16 inheritance. |

Correctness receipts (nprobe.json, adaptive.json, pq-margin.json, widths.json, kmeans-restarts.json,
fp16.json) are **derivations**, not performance baselines — they are re-derived per corpus/generation
by their own tests (paired series), not re-baselined here.

## The re-baseline pass (execute at dir-2 close)

Re-run the three headline baselines at the re-derived constants, on real-v1, backend-tagged:

1. **Per-ISA scan** — the SIMD bit-identical scan throughput, at the shipped PQ geometry (768d, pq_m=96).
2. **Query p50/p95/p99** — end-to-end latency at `DEFAULT_NPROBE=14` + the re-derived widths, across
   the query classes (topic, boundary, hybrid, threshold).
3. **S12 scaling lower bound** — the 1→4 in-process speedup ([D-080](../../docs/DECISIONS.md)) at the
   re-derived constants, so the deferred-to-transport verdict stands on honest single-host numbers.

**Composed-cost, folded in (issue #8):** a broad-τ threshold query now pays ε=0.30 overfetch *on top of*
nprobe pruning. Measure whether broad-τ threshold is the most expensive query class at the shipping
constants; if so, state it before S16 benchmarks it, and it raises the priority of the asymmetric
one-sided bound tightening (issue #8).

---

# Measured (S13 dir-2 close, final constants restarts=8 / nprobe=11 / block 16 KiB)

Receipt: [`rebaseline-real-v1.json`](rebaseline-real-v1.json) (`tests/rebaseline.rs`), on the frozen real-v1
corpus (768d all-mpnet, 3000 rows). **This is a measurement pass, not a tuning one — same engine, honest
constants, realistic data.**

## The two honest headlines

**1. The comparison that matters is not hash-v1 vs real-v1** (those measure different worlds — a degenerate
64d hash geometry vs a real 768d one). **It is real-v1-at-honest-constants vs the naive config a reader would
have picked on the *same* corpus.** On real-v1:

| | nprobe | p1 recall@10 | boundary zeros @nprobe=1 | verdict |
|---|---|---|---|---|
| **Honest** | 11 | **0.800** (holds the floor) | — | slower **and correct** |
| Naive | 6 | **0.600** (fails the floor) | 4 of 90 return nothing | fast **and wrong** |

The new numbers are slower **and** correct; the old ones were fast **and wrong on this geometry**. That is the
framing S16 publishes — not "the engine got slower" but "the engine is now measured at correct settings on
realistic data."

**2. The delta, decomposed by cause** (p50, first ISA), so a reader can attribute the change:

| Term | Δ p50 | What it is |
|---|---|---|
| **nprobe 6 → 11** | **+10.5 ms** | the price of a *real* tail floor (p1 0.60 → 0.80). Not a regression — the naive number was wrong. |
| **block 2 KiB → 16 KiB** | **+22 ms** | the price of **directory-openability at scale**, not query speed. 2 KiB reads fewer bytes on 3000 rows but carries a ~12 GB directory at a billion rows; the query cost is visible now, the directory cost only at scale (the S6 policy-bound situation, measurement can't see it small). |
| **768d itself** | *dominant, inherent* | the exact-rerank vector column is **3072 B/row — 32× the PQ codes and 12× the hash corpus's 256 B/row**. This is the corpus, not a tuning choice, and it is the dominant term for any byte-touching metric. |

## The numbers

- **Query latency (worst-ISA headline = scalar):** p50 **72.5 ms**, p95 **114.3 ms**, p99 **157.1 ms** (topic
  queries, warm). neon: 70.2 / 96.5 / 132.9 ms. The headline is the *worst* supported ISA, never the best's
  (determinism §7); kernels are bit-identical, so recall is ISA-invariant and only speed varies.
- **Per-ISA scan rate:** scalar **8,822 rows/s**, neon **9,390 rows/s** (compressed PQ codes, end-to-end).
- **Recall at the honest constants:** p1 **0.800**, mean 0.991, scan fraction 0.1915.
- **Storage (two-tier):** PQ codes 96 B/row, rerank vectors 3072 B/row (32× — the exact column the block-size
  re-derivation turned on).
- **S12 in-process scaling lower bound (re-confirmed at the current constants):** 1→4 speedup is scan-light
  **1.79×**, scan-heavy **0.79×**, GROUP BY **0.71×** — noisy and ≤ 1.6×, exactly [D-080](../../docs/DECISIONS.md)'s
  config-structural verdict (a single host adds no CPU/memory as shards grow; the sequential coordinator terms
  grow with shard count). Unchanged by the geometry constants — this measures coordinator fan-out overhead, not
  the recall constants. commit-RTT is backend-specific and geometry-independent; the D-080 receipt's numbers
  (p99 ≈ 1.7 ms, ≈ 1400 commits/s on the conformant local MinIO) stand (this pass's commit-RTT half was skipped
  — the `:9000` server available here is the non-conformant build).

## Composed broad-τ cost — issue #8's open question, answered

At the shipping constants (nprobe=11, ε=0.30), on real-v1:

| Query class | p50 | hits | physical bytes |
|---|---|---|---|
| topic top-k | 67.5 ms | 10 | 2.68 MB |
| **broad-τ = 0.2** | **279.8 ms** | 230 | 2.94 MB |
| narrow-τ = 0.6 | 77.9 ms | 15 | 2.76 MB |

**Broad-τ threshold IS the most expensive query class — 4.1× the topic latency.** The bytes moved are
comparable (the ε overfetch is a modest byte premium); the cost is *time*, spent exact-reranking the large
qualifying set a broad threshold admits (230 rows vs 10). This is known now, not discovered by a benchmark,
and it **raises the priority of the asymmetric one-sided bound tightening** (issue #8): tightening the +ε
margin shrinks the admitted candidate set without touching recall, which is the lever on this class.
