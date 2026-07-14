# Progress

Sprint gates from [PRISM.md](PRISM.md) Part IV. **A sprint is done when its gate is reproducible in CI — not when the code merges.** Each completed gate links the run that proves it.

| | Sprint | Gate | Status |
|---|---|---|---|
| **S0** | Executable reference slice + oracle harness | clean checkout passes CI; baselines checked in; README quickstart runs | ✅ **complete** |
| **S1** | Part format + recovery hardening | 10,000 randomized kill/reopen runs → old-or-new, never hybrid; all compat fixtures open, all corrupt fixtures rejected specifically; no untrusted length allocates unbounded | ✅ **complete** |
| S2 | Production ingestion + OTel GenAI schema | replaying acknowledged input loses no rows; offsets never advance pre-publication; one tenant cannot starve another | ⬜ **next** |
| S3 | Minimal SQL + scalar analytics | scalar-subset parity against an oracle; hybrid smoke queries identical through SQL | ⬜ |
| S4 | Hybrid partitioning + typed columns + data skipping | selective benchmarks read only eligible partitions; cross-tenant reads impossible even with malformed metadata | ⬜ |
| S5 | Immutable generations | queries available throughout a two-generation migration; rollback is catalog-only; no part decodes with the wrong codebook | ⬜ |
| S6 | CPU scan engine | bit/epsilon equivalence to the scalar oracle; zero heap allocation in the block loop | ⬜ |
| S7 | GPU compressed scan + rerank | GPU meets recall/tolerance vs the CPU oracle; p99 bounded under saturation; speedups end-to-end | ⬜ |
| S8 | Cost-based hybrid optimizer + full SQL semantics | SQL ≡ direct-executor results; fuzzing cannot bypass tenant policy; plan selection beats any fixed plan | ⬜ |
| S9 | Semantic GROUP BY at scale + novelty/drift | 100M-row filtered set < 10s single node; ARI ≥ 0.8 vs oracle; injected-novelty precision/recall ≥ 0.9 | ⬜ |
| S10 | Merge scheduler + mutations under load | sustained ingest → steady part count and merge debt; kills during merge/delete never expose partial results | ⬜ |
| S11 | Object storage + local cache | cold/warm/thrash SLOs; cache corruption repaired from remote; rerank fetch bytes bounded by the plan | ⬜ |
| S12 | Shared-nothing distribution | near-linear scaling; declared snapshot consistency under faults; a lost shard is documented, never silent | ⬜ |
| S13 | Production model plane | a model crash cannot corrupt or mislabel a part; every semantic value traceable to exact hashes | ⬜ |
| S14 | Security, governance, operability | independent tenant-escape review passes; deletion demonstrably absent; a new engineer ingests+queries in <15 min | ⬜ |
| S15 | Air-gap profile | full suite green in a no-egress container *including inference*; soak clean | ⬜ |
| S16 | Competitive benchmark + recall contract | third-party reproducible; SLOs name recall and cost, not latency alone; **losses published** | ⬜ |
| S17 | Beachhead product completion | partners query full telemetry with no application-side joins; operators complete drills from docs alone | ⬜ |

---

## S0 — complete

**Gate:** *clean checkout passes CI; baselines checked in; every engineer can identify the commit point and explain why GC is separate.*

**Proof:** [CI run #29302009336](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29302009336) — all eight jobs green on a clean checkout of `main`: `fmt`, `clippy -D warnings`, the full test suite in debug **and** release, the fault-injection matrix, the format-compatibility fixtures, the golden recall check, the baselines report, and — literally, by extracting the shell block out of `README.md` and executing it — the README quickstart.

**The commit point** is `Catalog::commit` in `crates/prism-part/src/catalog.rs`. It writes the snapshot file durably, then swaps `CURRENT` with a single atomic rename. Before that rename the old snapshot is live; after it, the new one is. There is no third state, and every kill point in the fault matrix is an assertion of exactly that.

### What is proven, and by which test

| Claim | Test |
|---|---|
| Data survives a reopen, byte for byte | `data_survives_a_reopen_byte_for_byte` |
| The write path is deterministic (content addressing means something) | `identical_ingests_produce_identical_parts` |
| An ineligible part is **never opened** — pruning is a fact about syscalls, not a claim | `an_ineligible_part_is_never_opened` |
| Probing fewer centroids scans strictly fewer rows | `probing_fewer_centroids_scans_strictly_fewer_rows` |
| The exact-rerank fetch budget is never exceeded | `the_rerank_fetch_budget_is_never_exceeded` |
| Approximate search meets its recall contract against the exact oracle | `approximate_search_meets_its_recall_contract_against_the_exact_oracle` |
| Probing every centroid reproduces the oracle exactly | `scanning_every_centroid_reproduces_the_exact_answer` |
| The committed golden corpus still means what it meant | `the_committed_golden_corpus_still_means_what_it_meant` |
| Unembeddable events are dead-lettered, never silently stored | `unembeddable_events_are_dead_lettered_never_silently_stored` |
| Merge preserves results and reconciles duplicates by the documented policy | `merge_preserves_results_and_reconciles_duplicates_by_the_documented_policy` |
| A query spanning two embedding spaces is **refused** (invariant 9) | `a_query_refuses_to_merge_scores_across_embedding_spaces` |
| Rollback is a catalog write, not a data rewrite | `rollback_is_a_catalog_write_not_a_data_rewrite` |
| GC never removes a referenced part | `gc_never_removes_a_referenced_part` |
| GC reclaims what no retained snapshot names | `gc_reclaims_parts_that_no_retained_snapshot_names` |
| Today's build opens the committed v1 format fixture | `todays_build_opens_the_committed_v1_fixture` |
| Every corrupt fixture is rejected with a **specific** error | `every_corrupt_fixture_is_rejected_with_a_specific_error` |
| Killing the writer at **every** durability boundary leaves old-or-new, never hybrid | `killing_the_writer_at_every_ingest_boundary_leaves_old_or_new_never_hybrid` |
| A killed merge changes nothing | `killing_a_merge_before_it_commits_changes_nothing` |
| A half-run GC never takes a live part with it | `killing_gc_midway_never_takes_a_live_part_with_it` |
| Crash orphans are invisible to readers and reclaimable only by GC | `orphans_left_by_a_crash_are_invisible_to_readers_and_reclaimable_by_gc` |

### The three permanent artifacts (Part II §7.4), started

- **`testing/compat/`** — a committed v1-format store (real bytes, not a generator) plus five corrupt fixtures: a flipped byte, a truncated column, a format version from the future, a codebook edited in place, and a string offset pointing past the end of its blob. Each must be rejected with an error that says *which byte lied*.
- **`testing/golden/`** — a 2,000-row corpus and the brute-force exact top-10 for every query. Two things are checked forever: that the exact answers have not **drifted** (the meaning of stored data cannot move silently) and what **recall** the approximate path buys, at what scan cost.
- **`testing/faults/`** — every durability boundary is a named kill point (`prism kill-points`); the matrix runs in CI on every commit, and `campaign.sh` is the randomized version that S1 turns up to 10,000 runs.

### Why GC is separate — the thing every engineer should be able to explain

Publication is one atomic rename (invariant 3), and a reader pins a snapshot for the whole life of its query (invariant 4). If reclamation ran inside the commit, a reader holding an older snapshot could have the objects underneath it deleted mid-query. So parts are never deleted by the thing that supersedes them; they are simply *no longer named*, and become invisible the moment `CURRENT` moves. Deleting them is a separate, explicit decision, made later, only about parts that no retained snapshot references (invariants 5 and 6).

This is also what makes crash recovery boring. A crash before the rename leaves an **orphan** — complete, checksum-valid bytes that no snapshot names, that no reader can see, and that nothing but GC will ever touch. There is no repair step, no log replay, no half-committed part to reconcile. There is only "the old snapshot" and "the new snapshot", and nothing in between.

### Honest findings from S0

Both of these are recorded in [DECISIONS.md](DECISIONS.md); they are the two bugs the artifacts caught before a human did.

- **A single probe can miss a query entirely.** At `nprobe=1` mean recall@10 across the standard query set is ~0.90 — but the *minimum* is **0.000**. One query's true neighbours all lived in a centroid we did not probe. Mean recall hides this; the minimum does not, so the recall report carries both. At `nprobe=4` (of 32) recall reaches 1.000 while touching ~14% of eligible rows.
- **The re-rank tier is bigger than the scan tier, at every scale.** On the S0 defaults, scanning 1,315 compressed rows moves ~10.5 KB, while fetching exact vectors for just 50 survivors moves ~12.8 KB. The 32× multiple is real and it dominates the storage bill, which is exactly why `baselines.json` reports PQ bytes and exact-vector bytes as separate numbers and never as one.

### Deliberate limits at S0

Single writer, single node, in-process. No SQL. Scalar loops only — no SIMD, no GPU; everything here is the *reference* implementation the fast paths will have to prove themselves equal to. A deterministic hash embedder, not a real model. Semantic grouping clusters re-rank survivors, not an arbitrarily large filtered set. Tenant isolation is a fused filter, not yet a physical partition boundary.

---

## S1 — complete

**Gate:** *10,000 randomized kill/reopen runs yield old-or-new snapshot, never hybrid; all compatibility fixtures open, all corrupt fixtures rejected with specific errors; no untrusted length allocates unbounded.*

### 1. Ten thousand crashes, zero hybrids

```
campaign: 10000 runs in 642s
  crashes that committed:    1465
  crashes that rolled back:  8535
  kill points exercised:
      1465  current.after_rename
      1445  part.after_rename_before_snapshot
      1439  part.after_fsync_before_rename
      1433  merge.after_part_before_commit
      1431  snapshot.after_write_before_current
      1395  part.after_write_before_fsync
      1392  gc.after_first_unlink

  hybrid snapshots: 0
```

Read the first two numbers together. **1,465 crashes committed data — and exactly 1,465 were kills at `current.after_rename`.** Not one crash at any *earlier* boundary committed anything, across ten thousand attempts.

That is the atomicity claim, measured rather than asserted. Publication happens at one instant — the rename of `CURRENT` — and a crash on either side of it leaves a complete world. Everything written before that instant and then abandoned is an **orphan**: real bytes, checksum-valid, named by no snapshot, visible to no reader, reclaimable only by GC. There is no repair step, no log replay, no half-committed part to reconcile, because there is no state for one to exist in.

Run it yourself: `PRISM_CAMPAIGN_RUNS=10000 cargo test --release -p prism-cli --test campaign -- --ignored --nocapture`. CI runs 400 on every commit and the full 10,000 nightly.

### 2. Every fixture opens; every corruption is named

**17 corrupt fixtures — 12 for v2, 5 for v1 — and every one is refused with an error that says which byte lied.** A generic `io error: failed to fill whole buffer` is a CI failure now, not a pass: an operator woken at 3am cannot act on it.

| Fixture | What it does | What the reader says |
|---|---|---|
| `bad-magic` | eight bytes overwritten | *does not begin with the PRSMPART magic; this is not a part file* |
| `bad-header-crc` | a bit flipped in the header | *header failed checksum: expected 0x7b1b6c8e, computed 0x766a5…* |
| `bad-body-crc` | a bit flipped in the body | *body failed checksum…* |
| `future-format` | version set to 999, header re-sealed | *part is format version 999, this build writes version 2* |
| `unknown-feature` | an unknown feature bit set | *requires feature bits 0x10000000000 that this build does not implement* |
| `unknown-codec` | codec id 99 | *column `pq_codes` uses codec id 99 (unknown)* |
| `unknown-rerank-encoding` | rerank encoding id 2 | *declares rerank encoding id 2, which this build cannot decode* |
| `absurd-length` | 4,294,967,295 centroid ranges | *…(at least 292 GB) but only N bytes remain; refusing to allocate on an untrusted length* |
| `block-checksum` | one byte inside one block | *column `body.data` **block 0** failed checksum* |
| `truncated-column` | file cut in half | *column `rerank_vectors` **is truncated**: block 3 needs bytes …* |
| `bad-offsets` | a string offset 1 TiB into a 13 KB blob, **all checksums repaired** | *offset pair (0, 1099511627776) is outside a 13772-byte blob* |
| `mutated-codebook` | a centroid edited in place | *generation does not hash to its own id* |

The corrupt fixtures are built by `scripts/partfmt.py` — an **independent Python decoder** of the binary format, not the Rust writer. A fixture generated by the code under test can quietly agree with a bug in it.

### 3. Nothing allocates on an untrusted length

`crates/prism-part/tests/fuzz.rs`, on every commit:

- 4,000 random byte-flip mutations of a real manifest
- every truncation length from 0 to the full manifest, plus trailing garbage
- 4,000 pieces of total garbage, half of them prefixed with a valid magic
- `u32::MAX` planted at every 4-byte-aligned offset in the body, **with both checksums repaired** — the adversary who *can* fix up a CRC, which is precisely the adversary a CRC cannot stop
- 3,000 checksum-repairing single-bit edits, asserting structural validation catches more of them than it lets through
- 200 column-byte corruptions, each of which must be caught by a block checksum

The reader must **decode it or refuse it with a specific error**. Never a panic, never an out-of-bounds index, never an allocation on the strength of a number it just read.

### What was built

| | |
|---|---|
| **Binary manifest** | magic, format version, byte order, feature bitset, per-column codec ids, self-checksummed header, checksummed body. A part that needs a feature this build has never heard of is **refused**, not guessed at. |
| **Block framing** | column files are sequences of 64 KiB checksummed blocks. A flipped byte condemns one block and **names it**; a ranged read fetches only the blocks that overlap the range. |
| **Rerank-tier descriptor** (D-003-resolved) | every part declares `{encoding_id, accuracy_contract_id}` and the reader **dispatches** on it. float32/exact is the only implementation; the descriptor is what makes changing it later a *migration* instead of a *format break*. |
| **Bounds-checked reader** | `format::Cursor` validates every length against the bytes actually present before reserving anything. |
| **`prism fsck`** | the offline validator. No catalog, no engine, no database. Reports **every** finding, not the first — plus two things a checksum cannot see: that stored vectors are unit-norm (a part that breaks this produces *silently wrong scores*, not errors) and that rows really are in `(centroid, event_time, event_id)` order. |
| **v1 still opens** | S0's parts still open, still verify, still answer — and `prism merge` migrates them forward to v2 without touching a single original byte. |

### The recall tail — S0's honest finding, now a gate

S0 reported mean recall@10 of ~0.90 at `nprobe=1` and noted the minimum was 0.000. S1 turns that observation into a mechanism.

The golden corpus now asks **104 questions in three classes**, and the classes are the whole point:

| class | n | what it is |
|---|---|---|
| topic | 32 | aimed at the middle of a behavioural motif — the easy case |
| **cluster-boundary** | 56 | half of one motif's vocabulary, half of another's, so the query lands *between* two centroids and its true neighbours are split across them |
| hybrid | 16 | meaning plus a scalar predicate |

At `nprobe=1`:

```
topic             n= 32  mean=1.000  min=1.000  returned nothing: 0
cluster-boundary  n= 56  mean=0.859  min=0.000  returned nothing: 5
hybrid            n= 16  mean=0.869  min=0.400  returned nothing: 0
```

**Topic queries are answered perfectly. Cluster-boundary queries fail outright, five times.** A benchmark that asked only topic questions would have measured recall 1.000 and concluded that one probe was enough. The overall mean — 0.904 — is how a system with five total failures in it gets described as "90% accurate".

So:

- every recall report carries `min`, `p1`, `p5`, `zero_recall_queries`, a per-class breakdown, and the **five worst queries by name** — a bug report, not a number;
- the default `nprobe` is **derived**: the smallest probe count whose *p1* recall clears 0.8. It comes out at **4** (mean 0.998, p1 1.000, zero failures, 14.9% scan fraction), and `testing/golden/nprobe-provenance.json` is the receipt. A test asserts `DEFAULT_NPROBE` still equals it *and* that no smaller probe count clears the floor — the constant cannot drift from its evidence;
- CI enforces `--min-p1 0.8` and `zero_recall_queries == 0`, not just a mean floor. A mean floor alone would have waved S0's failure straight through.

Adaptive per-query probing — scaling the probe count when the centroid margins are tight, which *is* the boundary case — is [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1), targeted at S6. It is deliberately not built here: it is a planner decision that must be costed against real kernel curves, and those do not exist until the CPU scan engine publishes them.

---

## S2 — next

**Production ingestion + the OTel GenAI schema.** From PRISM.md Part IV:

- OTLP ingestion mapping the GenAI semantic conventions into the §9 event model, plus a native streaming API and a Kafka source.
- Tenant auth, quotas, idempotency keys, and the duplicate policy (already documented and tested — [D-012](DECISIONS.md) — but not yet enforced at an admission boundary).
- A durable admission log, or a synchronous commit before ack. **Offsets must never advance before publication** (invariant 7).
- Batching by (tenant partition, model version) with backpressure; dead-lettering for schema *and* embedding failures (the embedding half already exists).
- A default `traces` schema in the box — *what gets embedded is a product decision, and the contract says to decide it deliberately.*

**Gate:** replaying acknowledged input produces no missing rows and documented duplicate behavior; offsets never advance pre-publication; one tenant cannot exceed quota or starve others.

**Carry into S2:** the event model is still the S0 slice (`event_id, tenant_id, event_time, event_name, cost, error, body`). S2 adds `observed_time`, `attributes`, `trace_id`/`span_id` and promoted attribute columns — and that is a **format change**, so it lands as v3 with a v2 fixture kept behind it, exactly as v1 is kept now.
