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

| Receipt | Measured at | Why superseded |
|---|---|---|
| `block-size.json` | `DEFAULT_NPROBE=6`, hash corpus | `sweep_block_size` builds its golden via `oracle::build` → default query (nprobe=6); its `bytes_read`, `read_amplification`, and `query_p50_ms` are all at the old probe count. The *chosen block size* (a bytes-moved trade-off) is robust to nprobe, but the absolute numbers are not. |
| `s12-scaling.json` | explicit `nprobe=16`, hash corpus | Used explicit probe counts, not the default, so it is not superseded *by the nprobe change alone* — but it predates D-081 and was measured on the hash corpus at a pre-real operating point. The scaling **lower bound** is re-run at the re-derived constants on real-v1 for an honest S16 inheritance. |

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
nprobe=14 pruning. Measure whether broad-τ threshold is the most expensive query class at the shipping
constants; if so, state it before S16 benchmarks it, and it raises the priority of the asymmetric
one-sided bound tightening (issue #8).
