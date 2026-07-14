# PRISM v2 — The Semantic Event Store
### Merged master document: product thesis + production architecture + sprint roadmap

**Status:** v2.0. Supersedes v1 (`06-prism-overview-and-roadmap.md`). This version merges the v1 product framing with the engineering corrections and discipline from the reviewed ARCHITECTURE/README/ROADMAP set (the "reference docs"): the executable-reference-slice-first approach, the two-level physical key, immutable generations, honest two-tier cost accounting, and gate-driven sprints are adopted from them; the market thesis, concepts primer, novelty/drift primitives, distributed semantic GROUP BY, air-gap profile, and GTM strategy are carried from v1.

**Audience:** the whole team, including junior engineers. Read Parts I–II before writing code. Parts III–IV are the build contract.

---

# PART I — WHAT PRISM IS AND WHY

## 1. One paragraph

PRISM is an analytical database for the data AI systems produce — prompts, completions, tool calls, agent traces, conversations. It stores billions to trillions of these events and lets you filter, search, cluster, and aggregate them **by what they mean**, combined in the same query with ordinary scalar predicates (tenant, time, cost, model) — one engine, one optimizer, no seam.

## 2. The lineage bet

| | Elasticsearch | ClickHouse | PRISM |
|---|---|---|---|
| Data class | Human text | Structured machine events | AI exhaust (traces, prompts, conversations) |
| Query shape | Find matching docs | Aggregate numeric columns | Cluster/filter/aggregate by meaning |
| Core structure | Inverted index | MergeTree (sorted columnar parts, sparse index) | Semantic MergeTree (partitioned, centroid-ordered parts, sparse centroid index) |
| Refuses to build | — | Per-token indexes | RAM-resident graph indexes |
| Hardware bet | Commodity RAM/disk | NVMe + SIMD | GPU + memory bandwidth |

The move each generation makes: refuse the expensive index; make brute-force cheap. PRISM applies it to vector search: no global neighbor graph — physically cluster data by meaning, keep a tiny centroid index resident, scan *compressed* codes within pruned ranges at bandwidth, exact-rerank bounded survivors.

**Epistemic rule (adopted from review):** "scan beats graph" is a *falsifiable workload hypothesis, not an article of faith.* Graph indexes win on small hot sets, very low k, latency-sensitive unfiltered queries. PRISM's expected advantage grows with cold data, high ingest, hybrid predicates, aggregation, and cost-constrained retention at billion/trillion rows. Our benchmarks must include the workloads where we lose, or they are marketing, not evidence.

## 3. The problem, concretely

Teams operating LLMs/agents at scale today: text/metadata in a columnar store, embeddings in a separate vector service, application-side ID joins, aggressive sampling (often retaining 1–10%) because the vector tier's economics fail at full volume — and the questions they actually have still go unanswered:

- **Hybrid search:** "traces from tenant X, last 6 hours, resembling this failure" — filters and similarity composed in one plan.
- **Semantic aggregation (flagship):** `GROUP BY` meaning — cluster 2B traces into behavioral motifs with counts, costs, failure rates per motif. The dashboard of the AI era.
- **Novelty & drift:** "what behavior exists this week that existed nowhere last week?" Novel jailbreaks, failure modes, intents — invisible to keywords and thresholds; visible as distance-from-known-structure.
- **Semantic security:** injection/jailbreak *variants* share meaning, not tokens.
- **Full retention:** economics that let customers stop sampling — which matters doubly because novelty/security queries are exactly what sampling destroys.

**Who buys:** LLM/agent observability and platform teams, AI security teams, conversation-analytics teams. Beachhead = observability, like both ancestors.

## 4. What PRISM is NOT

Not a RAG vector database (top-K for chatbots is a crowded market; we can do it, we don't lead with it — if a decision helps RAG but hurts analytical scans, analytics wins). Not a general OLAP replacement (good at scalar predicates because they compose; not trying to beat the incumbents at pure scalar). Not an ML platform (we run embedding models as versioned infrastructure, like tokenizers were to search engines; we don't train customer models).

## 5. The claims we make — and their honest form (adopted corrections)

1. **Compression is query acceleration:** 768-d float32 = 3,072 bytes; PQ-96 = 96 bytes (32×). Less memory traffic is real. But `3 TB/s ÷ 96 B = 31.25B codes/s` is a **roofline, not a service-level result** — lookup tables, selection, filters, kernel launches, transfers, skew, and rerank all take their cut. We publish measured p50/p95/p99 + recall at fixed hardware and cost. Never the bandwidth quotient alone.
2. **Storage economics are two-tier and we report both:** PQ codes ≈ 96 TB/trillion vectors, but exact rerank needs full vectors somewhere — float32 adds ~3.07 PB/trillion before text, scalars, replication. Options with different contracts: full vectors cold; float16 with a defined accuracy contract; re-embed-on-demand (storage↔latency+reproducibility trade); residual quantization (reconstructed, not exact). Every cost statement names scan-tier *and* rerank-tier bytes.
3. **Centroid counts are empirical:** `nlist`/`nprobe` are outputs of recall, skew, filter selectivity, and latency targets — with 4,096 equal clusters, one probe still touches ~244K rows/billion. No magic constants in docs or defaults without benchmark provenance.

---

# PART II — CONCEPTS PRIMER + ENGINEERING CHARTER

## 6. Primer (juniors: read until obvious)

**Embedding:** a model maps text → a vector (e.g., 768 floats) where similar meanings are near each other. Similarity = closeness (cosine; we normalize at ingest so dot product ≡ cosine and L2 ordering matches).
**The scale problem:** 3KB/vector × billions = terabytes→petabytes. Any design requiring raw vectors in RAM is dead on arrival.
**Product Quantization (PQ):** split vector into 96 sub-vectors; per-slot codebook of 256 patterns; store 1 byte/slot → 96 bytes. Distances on codes are approximate (via **ADC**: precompute query-vs-codebook lookup table; distance = 96 lookups + adds). Approximation is bounded by exact re-ranking of top candidates. *PQ is our compression codec; re-ranking is our accuracy warranty.*
**Coarse centroids (IVF):** k-means partitions vectors into clusters; a query scores the (tiny, resident) centroid set, picks `nprobe` nearest clusters, scans only those. Our sparse index of meaning — the semantic twin of MergeTree's sparse marks.
**Parts & merges:** writes create small immutable parts; background merges combine, re-cluster, apply tombstones, and (optionally) re-embed. Consequences: trivial crash recovery, trivial object-storage tiering, flat ingest cost vs dataset size, one maintenance mechanism. *Stability requires merge capacity staying ahead of ingest amplification — this is budgeted, not assumed.*
**Generations (the master invariant, adopted):** models, coarse codebooks, and PQ codebooks are content-addressed, immutable **generations**. A codebook defines the meaning of every stored byte — it is never mutated in place. Every part pins its (model, coarse, PQ) generation. Mixed-generation queries build one ADC table per generation and merge at exact-score time. **Scores from different embedding spaces are never merged without an explicit, validated bridge policy.**

## 7. Engineering charter (non-negotiable)

1. **Immutability is law.** No published part or generation is ever mutated. If you think you need to, you need a merge or a new generation.
2. **The CPU scalar path is the reference.** Every SIMD/GPU kernel has a scalar twin; CI proves bit/epsilon equivalence.
3. **Approximation is a tested contract.** Golden exact-search corpora per cohort; release gates specify recall@k at fixed scan bytes and latency.
4. **Three permanent artifacts from sprint 0** (adopted): a storage-format compatibility corpus (parts from every released format version), the exact-search golden corpus, and a fault-injection matrix that kills writers at every durability boundary.
5. **No global mutable state on the write path.** Ingest touches its batch, its new part, one atomic catalog commit.
6. **Every ticket names the invariant it preserves, the metric it changes, and the test that proves both** (adopted). Reject unbounded tickets ("add distributed search"); split until input/output/invariant/metric/failure-behavior fit one review.
7. **Benchmarks are end-to-end** — transfer, queue, selection, rerank included — never a favorable inner loop.

**Consistency invariants** (adopted, memorize): (1) parts never mutate after publication; (2) a catalog snapshot references only durable, checksum-valid parts; (3) publication is one atomic metadata transaction; (4) readers pin a snapshot for the query lifetime; (5) GC never runs inside the publication transaction; (6) old parts outlive max reader lease + grace; (7) source offsets/idempotency records advance with-or-after publication, never before; (8) codebooks/models are content-addressed and immutable; (9) no cross-space score comparison without a bridge; (10) checksums cover stored bytes end-to-end.

---

# PART III — PRODUCTION ARCHITECTURE

## 8. System shape

```
OTLP / Kafka / native producers
   → Admission (auth, quotas, idempotency, policy)
   → Versioned GPU embedding batches (separately supervised process)
   → Partition buffers (tenant-bucket × time-window × generation)
   → Immutable semantic part writer
   → Replicated metadata catalog (small, strongly consistent)
   → Object storage + local NVMe cache (parts; compute is disposable)

SQL / semantic API
   → Cost-based hybrid planner (scalar-first | semantic-first | interleaved)
   → Distributed scan executors (CPU SIMD / GPU PQ kernels, fused filters)
   → Exact survivor rerank (bounded budget)
   → Partial aggregation / global merge

Merge & model-evolution scheduler (budgets: I/O, write amplification, migration)
```

## 9. Physical layout (adopted two-level key)

**Outer partition:** `tenant-bucket × event-time window × semantic generation`. Why: hybrid queries almost always constrain tenant and time; partitioning makes isolation and retention *structural* (cross-tenant reads are physically impossible, deletes/TTL are partition drops), and generation partitioning makes migrations enumerable. Large tenants get dedicated buckets; small tenants share buckets with row-level policy masks.
**Inner part order:** `centroid_id × (optional residual bucket) × event_time × event_id` — semantic locality inside eligible partitions gives contiguous compressed scans.
**Part contents (self-contained, immutable):** typed compressed scalar columns; PQ codes in fixed-stride SIMD/GPU-aligned blocks; exact-vector blocks or references *stored separately from the hot scan tier*; **persisted centroid marks** `(centroid, first_row, row_count, byte_offsets)` so remote readers fetch only selected byte ranges; time min/max, tenant membership, zone maps, optional Blooms; generation IDs; checksummed block framing (damage localizes to a block); encryption key ID.

**Logical event model:** OTel-aligned. Core columns: `event_id, tenant_id, event_time, observed_time, event_name, body, attributes(map/promoted), trace_id, span_id, model_id+version, codebook_generation, centroid_id, pq_code, exact_vector_ref`. Dynamic values live in attributes, never event names.

## 10. Ingestion (adopted, 10 steps)

Authenticate/enforce policy → idempotency key + schema normalization → durable admission log (async ack) or synchronous part+catalog commit → batch by (model, version, input policy), embed on GPU → normalize vectors → assign centroid + PQ-encode with pinned generation → buffer by outer partition → sort by inner key, write immutable columns + checksums → upload/replicate → atomic catalog add; advance source offsets only after visibility. Embedding failures go to visible dead-letter/retry — **never silently store an event without its requested semantic columns.** Raw-body retention is policy-controlled (prompts contain secrets).

## 11. Query execution

Resolve semantic schema/generation; embed query once (cached by normalized query × generation) → mandatory tenant policy injected by the authorization layer (not removable by SQL) → partition/part pruning (time/tenant/zone maps) → centroid scoring, `nprobe` by recall/latency budget → marks → coalesced byte ranges → one ADC table per generation → streamed fixed-width scan with **fused scalar selection masks** → bounded candidate heap → exact rerank within a declared fetch budget → threshold/top-k semantics → partial aggregates → global merge.

**Four separate controls, all exposed in `EXPLAIN` (estimates and actuals):** `LIMIT`, `nprobe`, candidate width, rerank width — plus parts/blocks/ranges/bytes scanned, PQ vs exact bytes, and CPU/GPU route. The planner chooses among scalar-first (very selective predicates → small exact/PQ scan, skip centroid pruning), semantic-first (narrow centroid ranges, broad scalar), and interleaved.

## 12. Semantic analytics (carried from v1 — the category features)

**Semantic GROUP BY as a first-class distributed aggregate** — not just grouping rerank survivors. `GROUP BY semantic_cluster(embedding, k)` over arbitrarily large *filtered sets*: mini-batch clustering over quantized codes with exact-refinement of exemplars, per-cluster scalar aggregates (`count, avg(cost), countIf(error)`), and **distributed-mergeable partial states** designed before scale-out. Exemplar selection returns the most-central *actual events* — legibility is the product.
**Novelty & drift primitives:** `NOVELTY(embedding) AGAINST (baseline)` = distance to nearest centroid of a baseline period's snapshot (cheap; reuses existing structures); `SEMANTIC_DIFF(a, b, k)` = clusters with mass in B and none in A. Scheduled centroid-statistics snapshots per time partition power both. No exotic ML — distance-to-known-structure is simple, explainable, actionable.

## 13. Merge, evolution, distribution, security (adopted)

**Merges:** size-tiered per partition with concurrency/I-O/write-amplification budgets; reconcile duplicates/tombstones; optional re-embed to a new generation; atomic snapshot swap; retire old objects only after reader leases + grace. Migration is complete only when no active part references the old generation; rollback = catalog reference change, never data rewrite. Resumable from durable checkpoints; fairness so large tenants can't starve small.
**Distribution:** shard by tenant bucket, subpartition by time; Raft (or adopted transactional service) for the catalog only — consensus surface stays microscopic; immutable data on replicated object storage, NVMe as disposable cache; two-stage top-k (shard-local bounded candidates → coordinator exact merge); hedged reads, cancellation, retry dedup; admission control by estimated bytes scanned, GPU time, rerank fetches, aggregation state — not query count. Slow/lost shard produces *documented* partial-result behavior, never silent omission.
**Model plane:** the database owns model selection, versioning, batching, lineage, failure semantics — but inference runs in a **separately supervised GPU process** (a CUDA fault must not touch the storage engine). Drift/OOD/centroid-imbalance detection with seeded-alarm tests.
**Security:** tenant predicates injected below SQL; envelope encryption with tested revoked/rotated/missing-key behavior; **embeddings are sensitive and are not anonymization** — redact-before-embedding with recorded policy versions; deletion = tombstone parts + bounded merge/GC + object-version expiry, with deletion *provably* absent from snapshots, caches, and versions; audit covers query text, generations, scanned partitions, returned IDs, admin mutations.
**Air-gap profile (carried from v1):** `--profile airgap` compiles out egress; embedding model weights ship in the install artifact (no runtime model-hub downloads — the trap every naive AI product falls into); offline signed licenses with graceful degradation; signed offline update bundles; 120-day accelerated soak in CI. PRISM sells to the same air-gapped AI-lab/regulated buyers as FlockDB/LoomDB.

## 14. Production exit criteria (adopted verbatim)

Crash consistency under kill-at-every-fsync; deterministic recovery with missing/duplicate/corrupt/orphan objects; tenant isolation under parser/planner/cache/retry adversarial review; bounded memory under merge backlog and centroid skew; recall/latency/cost superiority on beachhead workloads at equal accuracy; online model/codebook migration with mixed generations and rollback; deletion/retention compliance including object versions; stable ingest under concurrent queries, merges, eviction, and node loss.

---

# PART IV — SPRINT ROADMAP (gates, not dates)

**How to run it (adopted):** every sprint has one outcome, junior-sized work packets, and an acceptance gate *reproducible in CI*. Advance on gates, not merged code. Maintain the three permanent artifacts (§7.4) from S0. Sequencing note vs the reference roadmap: an early minimal SQL surface is retained (design partners need something to touch early); the cost-based optimizer still lands after real kernel cost curves exist.

### S0 — Executable reference slice + oracle harness
Build (or adopt) the dependency-free single-node vertical slice: engine-owned embedding (deterministic hash-embedder for tests), k-means coarse + PQ training, ADC, immutable checksummed parts ordered by `(centroid, time, id)`, atomic `CURRENT` catalog, prune→scan→rerank→semantic-group query path, merge/re-embed, GC, CLI with physical-execution counters. Plus: CI; synthetic corpora (uniform, Zipf-skewed, tenant-skew, late, duplicates, empty/huge text); machine-generated baseline report (ingest rows/s, scan rows/s, recall@10, part-open time, merge amplification, bytes/row); EXPLAIN-style counters.
**Gate:** clean checkout passes CI; baselines checked in; every engineer can identify the commit point and explain why GC is separate.

### S1 — Part format + recovery hardening
Explicit binary manifest (endianness, feature flags, codec IDs, format version); persisted centroid marks with byte offsets; checksummed block framing; offline format validator; kill-point tests at every durability boundary; fuzz/property tests on manifests, offsets, lengths, NaNs, truncation.
**Gate:** 10,000 randomized kill/reopen runs yield old-or-new snapshot, never hybrid; all compatibility fixtures open, all corrupt fixtures rejected with specific errors; no untrusted length allocates unbounded.

### S2 — Production ingestion + OTel GenAI schema
OTLP ingestion mapping GenAI semantic conventions into the §9 event model + native streaming API + Kafka source; tenant auth, quotas, idempotency, duplicate policy; durable admission log or synchronous commit before ack; batching by (tenant partition, model version) with backpressure; dead-letter for schema/embedding failures. Default `traces` schema in the box (what gets embedded is a product decision — decide deliberately).
**Gate:** replaying acknowledged input → no missing rows, documented duplicate behavior; offsets never advance pre-publication; one tenant cannot exceed quota or starve others.

### S3 — Minimal SQL + scalar analytics (retained from v1)
pgwire + CLI; SQL subset: projections, filters, LIMIT, scalar aggregates + GROUP BY; vectorized scan with zone-map pruning; `embedding ≈≈ 'text'` top-k predicate wired to the slice's pipeline.
**Gate:** DuckDB-parity oracle on the scalar subset; hybrid smoke queries return slice-identical results through SQL; a design partner can be demoed.

### S4 — Hybrid partitioning + typed columns + data skipping
Outer partitions (tenant-bucket × time × generation); per-partition buffers and part-size targets; typed scalar columns, null maps, dictionary/delta/general compression; block min/max, low-cardinality sets, bounded Blooms; inner order preserved; execution counters extended (partitions, parts, blocks, PQ bytes, exact bytes).
**Gate:** selective benchmarks read only eligible partitions/blocks; recall within tolerance of S0; cross-tenant reads impossible even with malformed metadata/cache.

### S5 — Immutable generations
Content-addressed model/preprocessing/coarse/OPQ/PQ generation records; parts pinned to generations; codebooks trained from stratified/reservoir samples (never just the first batch); mixed-generation planning with per-generation ADC tables; create/canary/compare/rollback/retire lifecycle; the score-bridge rule enforced.
**Gate:** queries available throughout a two-generation migration; rollback = catalog-only; property/fault tests prove no part decodes with the wrong codebook.

### S6 — CPU scan engine
Kernel interface separated from reference impl; AVX2/AVX-512/NEON ADC with runtime selection + scalar fallback; fused scalar masks; allocation-free bounded top-k; mmap/direct-I/O, prefetch, bounded caches; profile skew, LUT cache behavior, NUMA.
**Gate:** bit/epsilon equivalence to scalar oracle on randomized inputs; zero heap allocation in the block loop; published throughput/latency/recall per architecture.

### S7 — GPU compressed scan + rerank (conditional routing)
PQ-table build, fused ADC/filter, top-k selection kernels; pinned buffers, streams, batching, cancellation; fp16/fp32 exact-rerank kernels with explicit tolerances; **CPU/GPU crossover cost model** calibrated by bytes, candidates, selectivity, queue depth, transfer locality; GPU memory admission + fair tenant scheduling; CPU fallback always.
**Gate:** GPU meets recall/tolerance vs CPU oracle; p99 bounded under mixed load and device saturation; reported speedups are end-to-end.

### S8 — Cost-based hybrid optimizer + full SQL semantics
Parser/binder for distance, threshold, top-k, `semantic_cluster`; null/tie/ordering/pagination/model-version semantics defined; mandatory-tenant-predicate rules; scalar-first/semantic-first/interleaved alternatives costed from S6/S7 curves; full four-control `EXPLAIN`; Arrow Flight SQL.
**Gate:** SQL ≡ direct-executor results; fuzzing cannot bypass tenant policy or crash parser/binder/planner; plan selection beats any fixed plan across the selectivity matrix.

### S9 — Semantic GROUP BY at scale + novelty/drift (v1 category features)
Distributed-mergeable cluster-aggregate states; mini-batch clustering over PQ codes with exact exemplar refinement; per-cluster scalar aggregates; `NOVELTY ... AGAINST`, `SEMANTIC_DIFF`; scheduled centroid-statistic snapshots per time partition.
**Gate:** semantic GROUP BY over a 100M-row filtered set < 10s single node; ARI ≥ 0.8 vs sklearn oracle on labeled synthetics; partial-state merge property tests; injected-novelty benchmark precision/recall ≥ 0.9.

### S10 — Merge scheduler + mutations under load
Size-tiered selection with I/O and write-amplification budgets; merge-debt/impurity/tombstone/migration tracking; reader leases + delayed reclamation; tombstone parts, replacement semantics, idempotent reconciliation; resumable merges; tenant fairness.
**Gate:** sustained ingest → steady part count and merge debt; random kills during query/merge/delete never expose partial results; re-embedding migration pauses/resumes/rolls back with exact progress reporting.

### S11 — Object storage + local cache
Content-addressed keys, conditional publication; coalesced ranged reads from persisted marks; NVMe block cache with checksums/quotas/attribution; hot (PQ+scalar) vs cold (exact vectors, bodies) tier separation; multipart recovery, integrity audit; object request/egress cost in EXPLAIN and admission control. **Two-tier cost reporting becomes a product surface here.**
**Gate:** cold-start/warm/eviction-thrash/remote-error SLOs met; cache corruption detected and repaired from remote; rerank fetch bytes bounded by the plan's declared budget.

### S12 — Shared-nothing distribution
Consistent catalog/lease service, snapshot IDs; tenant-bucket sharding with large-tenant split/reshard; shard-local bounded top-k + partial aggregates (S9 states); global exact merge; hedged reads, cancellation, retry dedup, backpressure; partition/failover chaos suite.
**Gate:** near-linear scaling through target node range; declared snapshot consistency under faults; slow/lost shard → documented behavior, never silent omission.

### S13 — Production model plane
Separately supervised batched GPU inference service (warmup, health probes); registered preprocessing/tokenizer/model hashes with output validation (dimension/norm/distribution); per-tenant model policy, redaction-before-embedding, rate limits, cost accounting; query-embedding cache; drift/imbalance/OOD detection; fallback policies that never mix spaces silently.
**Gate:** model crash/reload cannot corrupt or mislabel a part; every semantic value traceable to exact hashes; seeded drift alarms fire at controlled false-positive rates.

### S14 — Security, governance, operability
TLS/mTLS (hybrid PQC key exchange), scoped tokens, RBAC, immutable audit, key rotation; envelope encryption with revoked/rotated/missing-key tests; retention, legal hold, tenant export, deletion proof, object-version cleanup; full-phase metrics/traces (dogfooded into PRISM); backup/restore + catalog DR drills; overload controls by bytes/device-time/merge-debt/remote cost. Single-binary `prismd`; compose-up < 2 min; `prism doctor`.
**Gate:** independent tenant-escape + parser/planner security review passes; restore drills reproduce snapshots; deletion demonstrably absent per policy; a never-seen-PRISM engineer reaches ingest+query from README in <15 minutes.

### S15 — Air-gap profile
Egress compiled out; bundled model weights; offline signed licenses (graceful degradation — reads/writes never stop); signed offline update bundles with rollback; ±30-day clock-jump and 120-day accelerated soak in CI; SBOM + reproducible builds.
**Gate:** full suite green in a no-egress container *including inference*; soak clean.

### S16 — Competitive benchmark + recall contract (the launch gate)
Representative agent-telemetry corpora (real selectivity, text lengths, tenant skew, ingest concurrency); baselines: exact scan, tuned graph index, IVF-PQ, disk-ANN, columnar-DB vector functions — at held-constant hardware, recall@k, freshness, durability, replication, and *total retained bytes*; report ingest, merge debt, p50/p95/p99, recall, scan+rerank bytes, energy, object/GPU cost; cold/warm/mixed/delete-heavy/re-embedding/node-loss scenarios; publish configs and raw results **including losses**. Plus the "semantic observability" macro-benchmark spec (ingest OTel GenAI at X ev/s while serving semantic-GROUP-BY dashboards) published for others to run.
**Gate:** third-party reproducible; SLOs name recall and cost, not latency alone; beachhead go/no-go met without sampling away rare events.

### S17 — Beachhead product completion
Curated schemas/adapters for LLM calls, agent steps, tool calls, policy decisions, security events; saved hybrid queries (injection variants, drift, failure cascades, conversation cohorts); retention/cost controls, usage reports, recall/latency dashboards; runbooks (incident, migration, stuck merge, corrupt object, GPU loss, catalog recovery); design-partner migrations off the two-system seam, measuring removed application complexity.
**Gate:** partners retain and query full agreed telemetry with no application-side joins; operators complete all drills from docs alone; launch report shows user outcomes, not just engine numbers.

**Review checklist, every sprint (adopted):** does a crash expose untested state? can adversarial bytes cause unbounded allocation or tenant leakage? is every generation explicit? is approximate behavior measured against exact? are bytes scanned/rerank bytes/write amplification observable? are old format fixtures preserved or versioned? is cancellation/backpressure bounded? is the benchmark end-to-end?

---

# PART V — DISTRIBUTION & GTM (carried from v1)

OSS-first, ClickHouse playbook: engine Apache-2.0, no exceptions, never relicensed; monetization is operationally separable (managed cloud later; enterprise plane: fleet governance, compliance packs, air-gap support contracts) so the license is never the only defense. Local/self-hosted until revenue supports cloud; the air-gapped AI-lab/regulated buyer is a design center, not an edge case. Benchmarks published with losses are the marketing budget. The three numbers the company watches: **end-to-end scan rate at fixed recall**, **recall@k on the pinned suite**, **cost per billion events retained and queryable (both tiers)**. A sprint that moves none of these and isn't correctness/ops work should be questioned.
