# Progress

Sprint gates from [PRISM.md](PRISM.md) Part IV. **A sprint is done when its gate is reproducible in CI — not when the code merges.** Each completed gate links the run that proves it.

| | Sprint | Gate | Status |
|---|---|---|---|
| **S0** | Executable reference slice + oracle harness | clean checkout passes CI; baselines checked in; README quickstart runs | ✅ **complete** |
| **S1** | Part format + recovery hardening | 10,000 randomized kill/reopen runs → old-or-new, never hybrid; all compat fixtures open, all corrupt fixtures rejected specifically; no untrusted length allocates unbounded | ✅ **complete** |
| **S2** | Production ingestion + OTel GenAI schema | replaying acknowledged input loses no rows; offsets never advance pre-publication; one tenant cannot starve another | ✅ **complete** |
| **S3** | Minimal SQL + scalar analytics | scalar-subset parity against an oracle; hybrid smoke queries identical through SQL | ✅ **complete** |
| **S4** | Hybrid partitioning + typed columns + data skipping | selective benchmarks read only eligible partitions; cross-tenant reads impossible even with malformed metadata | ✅ **complete** |
| **S5** | Immutable generations | queries available throughout a two-generation migration; rollback is catalog-only; no part decodes with the wrong codebook | ✅ **complete** |
| **S6** | CPU scan engine | bit/epsilon equivalence to the scalar oracle; zero heap allocation in the block loop | ✅ **complete** |
| **S7** | GPU compressed scan + rerank | GPU meets recall/tolerance vs the CPU oracle; p99 bounded under saturation; speedups end-to-end | 🟡 **GPU-ready, GPU-off** — device-agnostic machinery built + tested against a CPU reference of the GPU route; the GPU gate is **not** claimed (no GPU runner) |
| **S8** | Cost-based hybrid optimizer + full SQL semantics | SQL ≡ direct-executor results; fuzzing cannot bypass tenant policy; plan selection beats any fixed plan | ✅ **complete** |
| **S9** | Semantic GROUP BY at scale + novelty/drift | 100M-row filtered set < 10s single node; ARI ≥ 0.8 vs oracle; injected-novelty precision/recall ≥ 0.9 | 🟡 **semantics complete; 100M<10s not claimed** — ARI, determinism, merge-invariance, and injected-novelty precision/recall all gated; the scale gate is measured honestly and filed (D-062), like S7's GPU |
| **S10** | Merge scheduler + mutations under load | sustained ingest → steady part count and merge debt; kills during merge/delete never expose partial results | ✅ **complete** |
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

**Proof:** [CI run #29304251262](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29304251262) — all ten jobs green on `main`, including the format fuzzer, a 400-run randomized kill campaign, both format-compatibility corpora, and the recall contract with a **tail** floor. The full 10,000-run gate runs nightly ([nightly.yml](../.github/workflows/nightly.yml)) and its local result is below.

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
- the default `nprobe` is **derived**: the smallest probe count whose *p1* recall clears 0.8. It comes out at **4** (mean 0.998, p1 1.000, zero failures, 14.9% scan fraction), and `testing/evidence/nprobe.json` is the receipt. A test asserts `DEFAULT_NPROBE` still equals it *and* that no smaller probe count clears the floor — the constant cannot drift from its evidence;
- CI enforces `--min-p1 0.8` and `zero_recall_queries == 0`, not just a mean floor. A mean floor alone would have waved S0's failure straight through.

Adaptive per-query probing — scaling the probe count when the centroid margins are tight, which *is* the boundary case — is [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1), targeted at S6. It is deliberately not built here: it is a planner decision that must be costed against real kernel curves, and those do not exist until the CPU scan engine publishes them.

---

## S2 — complete

**Gate:** *replaying acknowledged input → no missing rows, documented duplicate behavior; offsets never advance pre-publication; one tenant cannot exceed quota or starve others.*

**Proof:** [CI run #29341659110](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29341659110) — all twelve jobs green on `main`, including the S2 gate suite (real process deaths at three new admission kill points), the charter-C-1 constant ledger check, three format-compatibility corpora, and the recall contract with its tail floor.

The architect's instruction was to **write the contract before the code**, because the risk in this sprint was never the schema — it was the duplicate/replay semantics. [**docs/INGESTION-CONTRACT.md**](INGESTION-CONTRACT.md) is that contract, and every test in `crates/prism-cli/tests/ingestion.rs` asserts a clause of it. Where the contract and the code disagree, the contract is right and the code is a bug.

### 1. Duplicate behavior, documented and enforced

Three outcomes, and the distinction between the second and third is the whole point:

| key | content hash | verdict | why |
|---|---|---|---|
| new | — | **admit** | it is a new event |
| seen | **same** | **replay** — ack it, store nothing | the producer retried after an ack they never saw. They did **exactly the right thing**; punishing them for it is how you teach producers to drop data on error. |
| seen | **different** | **conflict** — dead-letter it | an id that was supposed to identify one event now identifies two. Last-write-wins is the seductive option and it is *wrong*: it silently rewrites history under a reused id, and makes behaviour depend on arrival order, which no producer controls. |

The honest limit is stated rather than hidden: the index is bounded (7 days of event time, hard-capped). Beyond it a replay is admitted as a new event and becomes a duplicate **row**, which merge reconciles by last-write-wins on `event_time` ([D-012](DECISIONS.md)). **Two mechanisms, one seam, and the seam is documented.**

### 2. Offsets may lag. They must never lead.

```text
  poll source                         offset stays at 100
    → admission (fairness, caps, cardinality, skew, quota)
    → idempotency (new? replay? conflict?)
    → WAL append + fsync         ←──  ACK. The events are now guaranteed.
    → embed
    → write immutable part
    → catalog commit             ←──  the events are now VISIBLE
    → record idempotency keys         (invariant 7: with-or-after publication)
    → mark the WAL record applied
    → advance the source offset       offset = 200
```

**An ack means durable. It does not mean visible.** Three new kill points drive real process deaths through this sequence in CI:

- **`ingest.after_embed_before_part`** — *the crash the architect named.* The batch is acked, the GPU time is spent, and the events exist nowhere but the log. Recovery brings them back **exactly once, with their embeddings** — the test asserts all twelve are semantically queryable afterwards, not merely present. An event restored without its vector would never match a semantic query, for reasons nobody could reconstruct later.
- **`wal.after_append_before_fsync`** — nothing was acked, so nothing may be lost. The source still owns the events and re-delivers them.
- **`ingest.after_publish_before_offset_commit`** — the data *is* published and the offset lagged. The source re-delivers all eight; idempotency recognises all eight as replays; nothing is duplicated. Lagging costs a redundant poll. **Leading would lose data permanently, which is why the offset is committed last.**

### 3. Quota *and* starvation — they are different failures

A quota stops a tenant exceeding their share. It does nothing about the tenant who is comfortably *within* quota and simply loud: 10,000 of their events arrive, and the quiet tenant's single event sits behind all of them.

So admission is **round-robin across tenants**, and the test is not "the big tenant was throttled" — it is **"the quiet tenant's position did not change when the loud tenant got 1,000× louder"**, which is the thing the quiet tenant actually notices.

### 4. Attributes, bounded before they existed

> *"`attributes` is where formats go to die — bound it before it exists."*

| limit | default | dead-letter reason |
|---|---|---|
| keys per event | 64 | `too_many_attribute_keys` |
| key length | 128 B | `attribute_key_too_long` |
| value length | 4 KiB | `attribute_value_too_long` |
| total attribute bytes | 16 KiB | `attributes_too_large` |
| **distinct keys per partition** | **512** | `attribute_key_cardinality_exceeded` |

**The last one is the only one that bounds the *shape* of the data.** The first four bound the size of one event; a tenant emitting `user_id_<uuid>` as an attribute **key** would grow a key dictionary the size of their traffic, carried in every manifest, forever. That is how a columnar format dies — not with one huge event, but with ten million tiny ones each introducing a column nobody will ever query.

So keys are a **bounded dictionary per partition**, held in the *manifest* (so "does this part contain key X?" is answerable without opening a column). A full dictionary **refuses** a novel key — and the dead-letter tells the tenant, in terms they can act on, that this is an instrumentation bug. A refused event **never widens the dictionary**, or a producer could exhaust a partition's budget with events that were never even stored.

Promotion of hot attributes to typed columns is [**issue #2**](https://github.com/Bobcatsfan33/PrismDB/issues/2), targeted at S4 — deliberately not built here, because promotion only pays once the block-skipping machinery it depends on exists.

### 5. Format v3 — designed to be the last cheap bump

Reserved feature bits (`PROMOTED_COLUMNS`, `PARTITION_META`, `COLUMN_COMPRESSION`, `ENCRYPTION`) with their numbers nailed down and all of them **refused** by this build; a **TLV extension section** with a required/optional bit, shipped with zero extensions defined so the first user is not also a format break; and four reserved manifest words that must be zero and are *refused*, not ignored, if they are not. S3's typed columns and S4's partitioning metadata land as **flagged extensions on v3**.

**Three released formats now open**: v1 (JSON manifest, S0), v2 (binary, S1), v3 (S2). v1 and v2 are committed *bytes* and are never regenerated — their whole value is that nothing since has touched them.

### 6. The charter amendment, and what it caught immediately

**C-1: no tuned constant without committed evidence and a test that binds them.** The ledger is `testing/evidence/registry.json`; the test checks it against the code **in both directions**, so a constant cannot drift from its receipt and a new one cannot be added without one. Two kinds, and the distinction is abuse-proof by design: `tuned` owes **evidence** (a file, a key, a rule), `policy` owes a **rationale** that points at prose the test verifies exists.

**The first thing it caught was ours.** `BLOCK_SIZE = 64 KiB` had been chosen in S1 because 64 KiB is what people choose. Deriving it required making it variable — and the measurement was brutal:

| block size | bytes read | read amplification | p50 |
|---|---|---|---|
| **4 KiB** *(derived)* | **69.2 MB** | **44.1×** | **6.8 ms** |
| **64 KiB** *(the S1 guess)* | 387.6 MB | **247.1×** | 29.7 ms |

A 300-byte centroid range and a 256-byte rerank vector had each been dragging a 64 KiB block off the disk. End to end, `prism bench` p50 fell **44.5 ms → 20.2 ms** and the scan rate doubled.

Making the constant variable also exposed a **latent bug**: `read_range` computed a block's logical offset from the *global constant* instead of the column's actual block size — silently correct for exactly as long as every column happened to be 64 KiB. Deriving a constant found a bug that assuming it had hidden.

### 7. A near-miss worth recording

Adding attributes to the corpus generator consumed extra PRNG draws, which shifted the stream and **silently changed the committed golden corpus**. `make-fixtures` regenerates the corpus *and* its expected answers — so the drift check would have gone on passing **by construction while testing nothing**, and S1's recall-tail finding would have quietly evaporated with it. The S2 fields now draw from a separate stream, and `testing/golden/v1/corpus.tsv` is **byte-identical to S1's**. A golden corpus that moves is not a golden corpus. ([D-023](DECISIONS.md))

### What S2 does not build

Named so nobody mistakes silence for completeness: **no network listener** (the OTLP *mapping* is real and tested; the server is S14) and **no Kafka client** (the `Source` trait has exactly Kafka's offset semantics and the file source implements them, so invariant 7 is tested through real process deaths). The gate is about semantics. The transport is a later sprint's problem. ([D-024](DECISIONS.md))

---

## S3 — complete

**Gate:** *parity against an oracle on the scalar subset; hybrid smoke queries return slice-identical results through SQL; a design partner can be demoed.*

**Proof:** [CI run #29348236729](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29348236729) — all fourteen jobs green on `main`, including the S3 gate suite (same-door counter parity, 8,000-statement tenant fuzz, every parser bound named, pagination under concurrent ingest+merge), the charter-C-1 constant ledger, and `golden-frozen` (charter C-2: committed bytes, not regenerated output).

The architect raised the real risk before we built anything: **the SQL path is a second door into the same engine, and it must be provably the SAME door.** So the gate is not "does SQL work" — it is "is SQL a compiler rather than a second engine", and it is enforced on the *counters*.

### 1. The same door, proven on the counters

Every gate query runs through **both** doors and asserts identical rows **and identical physical-execution counters**.

The counters are the point. If SQL ever grows its own scan, its own pruning, or its own idea of ordering, **the counters diverge before the results do** — and we would rather learn that from a counter than from a customer. Two doors into a database that disagree is a class of bug that takes years to find, because each door is individually self-consistent.

It is made structural, not merely tested: the **filter language lives in `prism-types::predicate`, not in the SQL crate.** The direct API builds exactly what SQL compiles to because there is only one thing to build. `sql_compiles_to_exactly_the_query_the_direct_api_takes` asserts the compiled `Query` *equals the hand-built one as a value*, before either is ever run.

The scalar subset also has a real oracle: a brute-force reference that materializes every event and filters it in a straight loop — no pruning, no zone maps, no columnar reads, no laziness. Slow and stupid on purpose, which is what an oracle should be.

### 2. Tenant policy is a shape, not a check

```
(whatever the user wrote)  AND  tenant_id = <session tenant>
```

The user's expression is a **subtree**. A subtree cannot widen the conjunction it is nested inside — not with an `OR`, not a `NOT`, not an alias, not parentheses, not a comment. **There is no list of escapes to enumerate and keep current, because there is nothing to escape *to*.** And the same tenant value drives *partition pruning*, so a query that somehow got past the row filter would still be reading only its own tenant's parts.

Nineteen hand-written escape attempts (`OR 1=1`, `NOT (tenant_id = 'mine')`, `tenant_id IN (…)`, comment-smuggling, alias games, case games, arbitrary nesting) and **8,000 fuzzed statements** — none escapes, none panics, none returns another tenant's row. Aliases don't even bind: `SELECT tenant_id AS t … WHERE t = 'other'` fails, because an alias is not a column and is not in scope in `WHERE`.

### 3. The parser is network-facing input

S1's bounded-allocation discipline, applied to SQL text — and **every bound named in its error**, because "syntax error" is not something an operator can act on.

| bound | limit |
|---|---|
| statement bytes | 64 KiB |
| tokens | 4,096 |
| **expression nesting depth** | **32** |
| `IN` list length | 1,024 |
| projections | 64 |

The depth counter is the one that matters. A recursive-descent parser without one is a stack overflow waiting for `((((((...))))))` — and a stack overflow is a **process death**, not an error. It cannot be caught, reported, or attributed to the query that caused it.

Also refused rather than ignored: a second statement (`SELECT 1; DROP TABLE events` — quietly parsing the first and dropping the rest is how a stacked-query injection becomes invisible), an unterminated comment, and an `ORDER BY` that contradicts the total order.

### 4. Pagination — pinned before anyone saw the syntax

[**docs/QUERY-CONTRACT.md**](QUERY-CONTRACT.md), written first, containing the sentence *"S8 may EXTEND these semantics. It may not contradict them."* — because by then there will be partners with cursors in their code.

- **One total order:** `(score DESC, event_id ASC)`. Always.
- **A cursor binds a snapshot and a position.** Paging reads *that snapshot*, not `CURRENT`.
- **An expired snapshot is an explicit error**, never a silently different answer.
- **No `OFFSET`** — and the error says why.

**The gate test pages a full result set while the store is actively changing underneath**: new events land and parts merge between *every single page*. The reader sees exactly the rows of the snapshot it started on — no duplicates, no gaps, nothing from the future.

**And it needed no new invariant.** Parts are immutable, a snapshot is a fixed set of them, ingest publishes a *new* snapshot without touching the old one, and merge writes *new* parts without touching the old ones. Pagination needed the invariants we already had to be *true*.

### 5. Two charter amendments' worth of discipline, and what they caught

**C-2 — golden corpora are frozen, immutable, versioned artifacts.** Arising from S2's near-miss ([D-023](DECISIONS.md)): a change to the corpus *generator* silently changed the corpus, and `make-fixtures` regenerated the corpus **and** its expected answers — so the drift check would have gone on passing *by construction while testing nothing*. Now: `testing/golden/` is versioned and checksummed, `make-fixtures` **verifies and never generates**, and a new corpus requires `scripts/new-golden-corpus.sh`, which **refuses to overwrite**, demands a reviewable `--reason`, and retains every predecessor — because every receipt names the corpus version it was measured against, and a receipt pointing at a corpus that no longer exists is not a receipt.

**C-1 — `DEFAULT_CANDIDATES` and `DEFAULT_RERANK` reclassified `tuned` and swept jointly.** They interact: the candidate width decides *who is allowed to be reranked*, the rerank width decides *how many actually are*. An independent single-axis sweep of either measures a cross-section of a surface and reports it as the surface. Result: **candidates 200 → 50**, rerank stays 50.

**But the honest part is that the recall floors do not bind at all.** Every point in the grid clears them — on this corpus, PQ's top-10 already contains the true top-10. Left there, the rule chooses `rerank = 10`, which would be overfitting to a synthetic corpus with unusually well-separated motifs *and* would quietly break the pagination that landed in the same sprint: **the paginated result set *is* the rerank survivor set**, so `rerank = 10` at a page size of 10 makes the first page the whole result and the cursor decorative. So the derivation carries a **policy** bound, `MIN_PAGEABLE_ROWS = 50`, and that is what actually selects the value. Measurement could not see it. Prose can. (Same shape as the manifest budget that constrains the block size.)

### Bugs found

- **Pagination never terminated.** The keyset skip was a tuple comparison, `(score, id) < (last_score, last_id)` — and a tuple compares its first element **ascending** while this order is `score DESC`. A row with an equal score and a *smaller* id was treated as "after" the cursor, so pagination rewound and repeated forever. Written out longhand now, with the reason.
- **An ungrouped aggregate over an empty set returned zero rows** instead of one row saying `0`, making "nothing matched" indistinguishable from "the query failed".
- **`ORDER BY` was silently ignored** rather than refused — which would let a caller believe they had asked for an order they did not get.

---

## S4 — complete

**Proof:** [CI run #29355868298](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29355868298) — all fifteen jobs green on `main`, including the new S4 gate (isolation as an I/O property, and the promotion dual door), the charter-C-1 constant ledger, the charter-C-2 frozen-corpus byte check, and the baselines job whose **tail** recall floor is the thing that caught [D-033](DECISIONS.md). 238 tests.

**Gate:** *selective benchmarks read only eligible partitions/blocks; recall within tolerance of S0; **cross-tenant reads impossible even with malformed metadata or a poisoned cache**.*

### 1. "Physically impossible" is now an I/O property

The architect refused to let this stay a slogan: *define it as an I/O property — a query's execution trace never touches a byte range belonging to another tenant's partition.*

That has a sharp consequence, and it reshaped the sprint. **Until S4, pruning opened every part's manifest** to decide which parts to skip — so a tenant-A query *already* read tenant B's bytes, and one corrupt part broke every query in the store. **Pruning that must open a manifest to decide whether to open a manifest is not isolation.**

So the partition key, tenant list and zone map moved into the **catalog snapshot**, above the parts ([D-028](DECISIONS.md)).

**The gate test does the strongest thing we could think of:** it fills every non-`alpha` partition with unreadable garbage — manifests, columns, every byte — and then runs alpha's scalar, semantic, hybrid, aggregate and time-bounded queries. All of them answer correctly. **Because they never looked.**

And the shredded tenants get an **error**, not silence. Their data really is gone, and reporting zero rows would be a lie.

**A second property fell out for free:** blast radius is localized **per column** as well as per tenant. Corrupting `pq.codes` in one of bravo's parts breaks bravo's *similarity search* and leaves bravo's `COUNT(*)` working — because a count does not read the compressed codes. "Tenant bravo cannot run similarity search on this partition" is actionable. "The store is corrupt" is not.

### 2. The shared-bucket seam — chosen deliberately, logged

In a shared bucket, part-level metadata describes *the bucket*, not the tenant: one time range, one cost range, one union key dictionary, one tenant list. Every one of those tells tenant A something about tenant B.

**What is scoped (enforced and tested):** every part carries a **per-tenant section**, and a query reads its own and no other. *"Does this part contain key X?"* is answerable **per tenant**. A **zone map is a zone map for one tenant** — which closes the leak *and* prunes better, the pleasant case where the secure thing is also the fast thing.

**What is not hidden (documented and accepted):**

> An operator with **raw disk access** to a shared bucket can see which tenants share it, and the union of their attribute keys. **No query can.**

The dictionary has to be there — it is what *decodes* the attribute column. The tenant list has to be there — it is what *prunes* the part. Mitigations are real: a **dedicated bucket** (`--dedicated whale`) shares a part with nobody, and a dedicated bucket holding two tenants is **refused at commit**, because if it were accepted every isolation claim resting on it would be false and nothing would notice. S14's envelope encryption closes the disk layer properly. ([D-030](DECISIONS.md), [query contract §8](QUERY-CONTRACT.md))

Bucket assignment is **SHA-256**, not a fast hash: a tenant must not be able to *choose* their bucket by choosing their id. An attacker who can steer themselves into a chosen victim's bucket has turned a metadata question into a targeting one.

### 3. Promotion is a dual door — the gate that matters most

Promotion is a **versioned, generation-like schema event, never an in-place rewrite**. The typed column and the attribute map **coexist across parts of different ages**, and a merge migrates a part forward — the same mechanism as every other migration here.

The equivalence test: two stores, identical but for promotion. Every query must return **identical rows and identical logical counters** — same parts opened, same rows scanned, same rows passing the filter. If promotion changed what the engine *considered*, it would be a different query wearing the same text.

`physical_bytes_read` is the **one counter that legitimately differs**, and the test asserts it differs **downward** — because if promotion did not read fewer bytes, it bought nothing:

```
SELECT count(*) FROM events
 WHERE attributes['gen_ai.system'] = 'anthropic'
   AND attributes['gen_ai.usage.input_tokens'] > 2000

  plain      count=186   physical_bytes = 181,489
  promoted   count=186   physical_bytes = 118,440    (-35%)
```

**Asserting the win rather than assuming it caught a real bug instantly:** the first implementation re-read all three column files *on every row*, so promotion read **more** bytes than the map it replaced. It also exposed that `COUNT(*)` was materializing every column of every row to answer a question about the *number* of rows. It now materializes nothing.

Promotion is **invisible to an event**: a promoted part reads back byte-identical to a mapped one. It is a storage decision, not a schema change — if it were observable, every equivalence in the system would quietly stop holding. And a key used with two value types is **refused**: a promoted column is typed, and a key that is an int on one row and a string on another is a map entry pretending to be a column.

### 4. Pruning, and the fuzz corpus one layer down

- **Zero false negatives**, as a property test over 10,000 randomized part-metadata/query pairs. Pruning may be too generous — that costs a scan. It may never be too eager: a part excluded that held a matching row is a **lost row**, and pruning that can lose a row is not pruning, it is sampling.
- **Partition metadata is untrusted input.** The S3 fuzz corpus moved down a layer, as planned: 5,000 byte flips, every truncation length, `u32::MAX` planted at every aligned offset, 5,000 pieces of garbage. Decode it, or refuse it with an error naming the byte. Never panic, never allocate on a number a stranger sent. A zone map with `time_min > time_max` is **refused** — a zone map that cannot be true is worse than no zone map, because it is *trusted*.
- **Data skipping:** a query for one tenant in one hour, over a store with 20+ partitions, opens **at most 2 parts**. As a fact about the counters.

### 5. Charter amendment C-3 — and the constants are now marked honest

Formalizes the pattern that has appeared three times: **a sweep declares its policy bounds, with a written rationale for why measurement cannot see them**, and **an optimum on the boundary of its grid is presumptively an artifact** — expand the grid or find the missing constraint before committing the receipt.

Every receipt now records the **golden-corpus version** that produced it, and every constant derived on a hash-embedder corpus is marked **`corpus_conditional: true`**. That is honest, and it is not a fix. The hash embedder exists so tests are reproducible with no weights and no network — it is the right tool for that and the **wrong corpus to tune an index on**. Its motifs are unusually well-separated, which is exactly why the `(candidates × rerank)` sweep found the recall floors *did not bind at all*.

**[Issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3)** files the fix: build a real-embedding golden corpus (small open model, CPU, checksum-pinned, frozen under C-2) and re-derive every C-1 sweep against it. **Not deferred to S13** — S13 is the production GPU model plane, which is not a prerequisite for running one small model once, offline, and committing its output.

### The bug S4 found in S0's code

**`p1 recall@10` fell from 1.00 to 0.60, and raising `nprobe` did nothing at all.**

That second half is what named it. If rows were being *missed*, probing harder would find them. It didn't — so they were not being missed. On the same corpus, with the same codebook, the pre-S4 and post-S4 engines scanned **the same 3,880 rows**, considered **the same 50 candidates**, and returned **the same top score** — with **different event ids**.

The bounded candidate heap broke distance ties on `(part, row)`, under a comment calling that *"deterministic"*. It is — **in the layout**. Which is the exact error [D-008](DECISIONS.md) corrected in the final sort, surviving in the one place D-008 never reached. The heap is *bounded*, so it does not merely order the answer, it decides **which tied rows are allowed to be answers at all**. Repartitioning changed nothing about the data and everything about the layout, and the answers moved. Ties are not exotic here: real telemetry repeats bodies verbatim, so exactly-equal distances are ordinary and a top-k is routinely a choice among hundreds of tied rows.

Ties now break on `event_id`, so the candidate set is a function of the data. **It costs 19%** (bench `query_p50` 21.9 → 26.1 ms), which is the honest price of an answer that does not change when the database tidies up.

**Every other suite passed** — including the golden recall gate, because the golden store is a *single part*, and a single part has no layout to disagree about. The bench was the only thing that could see it, and the only reason it did is that the recall report carries `p1` and `min` and not just the mean. **The mean was 0.965.** ([D-033](DECISIONS.md))

Two tests now bind it, and both were verified to **fail against the old code** before being committed. The first version of the layout test varied only the time window — it passed against the bug, because every window layout scans tied rows in time order. It had to vary the *ingest batching* too before it could see anything. A test that cannot fail is not a test.

### Bugs found

- **The candidate heap chose its rows by address** ([D-033](DECISIONS.md)) — the one above.
- **Promotion read *more* bytes than the map** — the columns were re-read per row. Caught by asserting the win.
- **`COUNT(*)` materialized every column of every row.** It now materializes none.
- **The embedding-space filter stopped counting its own pruning** when catalog pruning arrived, so a narrowing the caller could not see in the counters was a narrowing they could not audit.
- **An unknown CLI flag was silently ignored** — `--promot gen_ai.system` created a store without the promotion its operator asked for ([D-032](DECISIONS.md)).
- **The build refused its own parts** when the S4 extension was not registered in `SUPPORTED_EXTENSIONS` — which is the required-extension mechanism working exactly as designed, loudly, at the commit that would have published them.

---

## S5 — complete

**Proof:** [CI run #29367911566](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29367911566) — all seventeen jobs green on `main`, including the two new gates: **the S5 gate** (migration, wrong codebook, bridges, DEGRADED alarms) and **charter C-4** (the answer is a function of the data, not the layout). 255 tests.

**Gate:** *queries available throughout a two-generation migration; rollback is catalog-only; property/fault tests prove no part decodes with the wrong codebook.* Plus the architect's two additions: **no two spaces' scores merge without a declared bridge**, and **drift baselines are generation-scoped, with DEGRADED rather than silence.**

Contract first, as always: [docs/GENERATION-CONTRACT.md](GENERATION-CONTRACT.md) was written before the lifecycle existed.

### 1. The lifecycle, and why a crash in the middle of it is boring

`create → canary → compare → promote → migrate → complete → retire`, and **every transition is one catalog commit**. Not "mostly". The generation *record* is content-addressed and immutable — it **is** its codebooks — so the lifecycle *state* lives in the snapshot, where it belongs ([D-037](DECISIONS.md)). Which buys the hard parts for free: there is no half-migrated flag, no repair path, no state in a writer's memory, and **rollback restores the states along with the parts, because they are the same object.**

The gate test walks every step and asks a real question at each one. It answers at all of them, including with two live generations.

**No part is ever decoded with the wrong codebook.** The failure mode here is not a crash — it is a **plausible wrong answer**: a PQ code read against the wrong codebook still produces a number, and the number looks fine. So the catalog's generation and the part's own manifest must agree, and a poisoned catalog is **refused**, not decoded.

**Retire is the only step that forecloses anything**, so it is last, and it refuses while any *retained snapshot* still names the generation — because a rollback target that cannot be read is not a rollback target.

### 2. Drift baselines are generation-scoped, and never silent

A baseline is a statement about a distribution **in one embedding space**. When the space changes underneath it, the baseline is not stale — it is **meaningless**, and invariant 9 forbids comparing across it.

So a migration is **not complete** when no part references the old generation. That definition is *necessary and not sufficient*, and shipping it alone would have broken drift detection **silently**: the alarm would have gone on producing numbers, every number would have been nonsense, and nobody would have been told. `migration_status` refuses to say `complete` while a baseline still points at a dead space.

And when a baseline **cannot** be rebuilt — because rebuilding means re-embedding history, re-embedding needs the raw bodies, and raw bodies expire under retention because prompts contain secrets — the alarm goes **`DEGRADED`**, names the reason, says how many rows are going unwatched, and **`prism drift check` exits non-zero**. An alarm that is not running is an incident, not a quiet day.

> **A drift alarm that quietly stops firing is worse than one that was never configured, because a configured alarm is trusted.**

Getting the DEGRADED *rule* right took a failing test. The naive version — "degraded if this generation's rows were redacted" — is wrong in the direction that stays silent: redaction does not invalidate an existing baseline, because the vectors are still there and the space has not moved. What breaks is the **new** generation's baseline, which could only be calibrated on whatever rows *happened* to survive. That is not a baseline; it is a biased subset wearing a threshold ([D-038](DECISIONS.md)).

### 3. Bridges fuse ranks, never scores

A cross-space query is **refused** by default, and that is the correct behaviour: a cosine of 0.83 in one model's space and 0.83 in another's are two different numbers that print the same. A **bridge** is a catalog-registered, validated declaration that two spaces may be answered together — and the only policy is `rank_fusion`, which **does not merge scores at all**. Each space answers natively, inside its own geometry, and the *rankings* are fused. A rank is unitless. This **obeys** invariant 9 rather than working around it, and a bridged answer is **labelled**, because letting it pass for a native one would be a lie by omission ([D-039](DECISIONS.md)).

### 4. Charter C-4 — and the audit that found two live violations

> *"any bounded or truncating selection breaks ties on logical row identity, never on physical position."*

The audit swept every bounded structure in the engine. It found **two live violations in code that passed every test in the suite**:

- **The training sample** was a reservoir keyed on *index into a vector built by reading parts in catalog order*. Same rows, different layout → **different codebook** → a different meaning for every byte in the store. This is D-033's disease in the place it would have been hardest to ever notice, and it is what S5 is *built on* ([D-034](DECISIONS.md)). Now: bottom-k by `sha256(seed ‖ event_id)`, stratified by **tenant** — every key logical, so the sample is a pure function of the *set* of rows.
- **Merge duplicate reconciliation** broke `event_time` ties with *"the later part wins"* — physical position, stated as a feature in a comment ([D-035](DECISIONS.md)). Now the content hash wins, which is a total order on the data.

**The permanent gate:** one frozen logical corpus (C-2), materialized **four ways** — different time windows, different ingest batchings, before and after a merge, **one part to fifty-five**. Every golden query must answer byte-identically across all of them, and the same rows must train a **byte-identical codebook** (asserted on the generation id, which *is* the content hash of the codebooks).

### 5. What the layout gate found that nothing else could

Three of the four layouts failed it, and the cause was not a tie-break.

**The bootstrap generation is trained on the first batch** — because on the first batch, the first batch is all there is. No amount of C-4 discipline fixes that: you cannot train on data that has not arrived. So the gate states what is actually true, in two halves ([D-041](DECISIONS.md)):

1. **The exact path is layout-invariant always**, even on a provisional store. It uses no codebook, so there is nothing to hide behind.
2. **The approximate path is layout-invariant once the store is settled** — trained from the whole store and migrated onto. Which is what the lifecycle is *for*.

And on the way it exposed a second, fixable violation: the training sample was stratified **by partition**, and a time window is a *store configuration*. Two stores with identical rows and different window sizes trained different codebooks. **The strata themselves have to be logical**, or keying the sample on `event_id` buys nothing. A tenant is a fact about a row; a time window is a fact about a config file.

### 6. `DEFAULT_NPROBE` rose from 4 to 6, and the engine did not get worse

Fixing the sample removed a **lucky input order** that k-means++ had been quietly relying on — the training vectors arrived in corpus order, which handed it a fortunate first point and produced centroids that happened to align with the corpus's motifs. Recall promptly fell below its floor.

**The recall we had been reporting was, in part, a coincidence of the input order.**

`KMEANS_RESTARTS` (5 seeded inits, best by inertia) makes the codebook depend on the data instead of on a draw — and it is a **tuned** constant with [its own receipt](../testing/evidence/kmeans-restarts.json), because it was chosen by measurement. Its rule had to be rewritten once: the obvious "pick the best point" chose a **singleton lucky dip** (3 restarts needed only 3 probes; nothing larger reproduced it). The rule is now **the smallest restart count that begins a plateau**, because *we are choosing a method, not a lottery ticket* ([D-042](DECISIONS.md)) — C-3's "a boundary optimum is an artifact", generalized to singletons.

Then `DEFAULT_NPROBE` was re-derived against the new geometry, exactly as directive 3 demands, and moved **4 → 6**; mean scan fraction 0.146 → **0.203**. What that bought ([D-043](DECISIONS.md)):

| | before | after |
|---|---|---|
| `p1 recall@10` at the default | 0.80 | **1.00** |
| codebook depends on layout | **yes** | no |
| loudest tenant writes the codebook | yes | no |

**A number that goes up when you stop fooling yourself is the correct number.**

### 7. Receipts are generation-conditional (directive 3)

Every tuned entry now carries `generation_conditional: true`, and every receipt records the **generation id** it was measured under. This is not hypothetical — S5 lived it twice: the receipts had to be re-derived after the sampler changed, and again after the strata changed. **A receipt that describes an engine which no longer exists is not evidence; it is decoration.**

### Bugs and findings

- **The training sample depended on the layout** — the codebook, and therefore the meaning of every byte in the store.
- **Merge reconciled duplicates by address.**
- **The strata were physical** (partitions), which reintroduced the same defect one level up.
- **k-means++ was riding a lucky input order**, and the recall floor had been flattering it since S0.
- **The restart sweep's first rule picked a lottery ticket**, and had to be rewritten to prefer a plateau.
- **`TRAIN_SAMPLE_MAX` had steered behaviour since S0 and was never in the ledger** — a C-1 hole the audit closed.

---

## S6 — complete

**Proof:** [CI run #29383807911](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29383807911) — green on `main`, including the S6 gates on **two architectures**: `determinism` (x86, scalar == AVX2) and `determinism-arm` (`ubuntu-24.04-arm`, scalar == NEON), plus `no-alloc` (counting allocator), `unsafe-inventory` (grep gate), and `avx512-compiles` (the gated kernel type-checked on x86). 272 tests.

**Gate:** *SIMD kernels with a scalar twin CI proves equal; allocation-free hot loop; adaptive nprobe; unsafe inventoried; per-ISA end-to-end numbers.*

Contract first, as always: [docs/DETERMINISM-CONTRACT.md](DETERMINISM-CONTRACT.md) was written before the first kernel, and charter **C-5** gives it weight — *the answer is a function of the data, not of which instruction set computed it.*

### 1. Bit-identical, not epsilon-close — and why that was achievable

The determinism gate is the strong form: every kernel returns **byte-identical answers**, the same ordered ids and the same scores, over the layout-variant fixtures. Not a tolerance.

That is achievable because of how the ADC distance is *defined* ([D-044](DECISIONS.md)): a per-row ascending sum with no FMA, vectorized **across rows — one row per lane**. Each lane runs the identical scalar chain, and lane-wise IEEE addition is correctly rounded and identical to scalar, so the gather-and-add is bit-identical on AVX2, NEON and AVX-512. The rerank dot product stays scalar on every ISA (it runs over tens of rows, not millions), so it too is ISA-invariant. **A whole query is therefore bit-identical across ISAs.**

The x86 CI runner proves scalar == AVX2; the ARM runner proves scalar == NEON; between them every shipping kernel is checked against the reference on real silicon. A boundary-tie stress corpus proves selection-invariance where a one-`ulp` disagreement would flip which tied row survives the bounded heap.

**Honest speed finding:** at this engine's shape (`m=8`, `dim=64`) the NEON kernel with manual gather is *not faster* than the compiler's autovectorization of the scalar loop — baseline NEON 28.2 ms vs scalar 27.8. The sprint's deliverable is **bit-identity and the contract that enforces it**, not a speedup; the headline quotes the worst ISA (NEON) per C-5. A real win wants wider `m` or a hardware gather, and will still have to pass this gate.

### 2. The hot loop allocates nothing — proven

A counting allocator offers the real `TopK` 50,000 rows and the real kernel a full range and asserts **zero** allocations. This forced the candidate top-k to hold `(part, row)` indices instead of an owned `event_id: String`, borrowing the id out of the resident scalar column for the tie-break ([D-045](DECISIONS.md)) — allocation-free *and* faster, having dropped a string clone per candidate.

### 3. Adaptive probing — monotone, and honest about the corpus

A boundary query probes *above* the base `nprobe`, never below ([issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1)). So recall can only improve and every existing receipt stays valid as a floor — which is why the receipts are measured with adaptive off. The margin (0.05) is corpus-conditional: on the hash corpus the floor at the shipping base is already met, so measurement can only see *cost*, and a policy cost bound selects the value while the mechanism is *validated* by recovering a deliberately-starved base's tail ([D-046](DECISIONS.md)). The cost-reduction direction waits for a real corpus ([issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3)).

### 4. `unsafe` starts, and is inventoried

SIMD intrinsics and mmap are the first `unsafe` in the codebase. Every block is in [UNSAFE-INVENTORY.md](UNSAFE-INVENTORY.md) with its safety argument and its covering test, and a CI grep gate fails if an `unsafe` token exists without an entry. mmap is read-only over immutable parts; a **truncated file under mmap** names its column and block *by construction* — the map's length is the file's real length, every access is bounds-checked against it, and the `SIGBUS` is unreachable because the check fires first ([D-047](DECISIONS.md)). A fault test truncates a real framed column at many lengths and proves the process survives.

### 5. The re-run that caught a latent bug

Directive 5 — re-run the C-1 sweeps against the SIMD engine — did exactly what it exists to do. Every **recall**-derived constant reconfirmed *exactly* (`nprobe=6`, `candidates=50`, `rerank=50`, `restarts=5`), because the kernels are bit-identical: the answers did not move, only their speed. But the block-size sweep **panicked** — no candidate fit the manifest budget — because S4/S5 had grown the manifest with fixed-overhead extensions, and the sweep was budgeting the whole manifest instead of just the **block directory** it cares about. Corrected to budget the directory term alone, `BLOCK_SIZE` re-derived **4 KiB → 2 KiB** ([D-048](DECISIONS.md)). A receipt that describes an engine which no longer exists is not evidence, and the re-run is what keeps that from happening.

### 6. Per-ISA numbers, worst-quoted

`baselines.json` grew an `isa` dimension — end-to-end p50/p95/scan-rate per instruction set, never kernel-only. The headline latency is the **worst** supported ISA's, because a number that is only true on your fastest machine is false on the one your customer runs.

### Findings

- **The block-size budget was budgeting fixed overhead**, not the directory that scales — invisible until S4/S5 extensions grew the manifest and the re-run panicked.
- **NEON manual-gather does not beat scalar autovectorization** at `m=8` — reported honestly rather than hidden behind a kernel-only number.
- **AVX-512 ships off**: no CI runner can execute it, so it stays behind `experimental-avx512` and is compile-checked only, per C-5's "an ISA CI cannot execute does not ship enabled."

---

## S7 — GPU-ready, GPU-off (the gate is **not** claimed)

**Proof:** [CI run #29423076591](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29423076591) — green on `main`, including the S7 device gate (selection-identity, route-flip pagination, fault-degrades-to-CPU), the fp16 accuracy-contract check (tolerance ≥ 2× the gap, selection stable), the unsafe-inventory grep gate, and a `cuda`-feature compile check. 288 tests. The GPU gate itself is **not** among these — there is no GPU to run it.

**Directive 2 named the honest outcome up front, and this is it.** CI GPU capacity is a sprint deliverable, and this environment cannot deliver it — no CUDA hardware, no cloud credentials to provision a runner. Per the architect's own fallback, **S7 pivots to "GPU-ready, GPU-off" and does not claim the GPU gate** ([D-053](DECISIONS.md)).

What that means precisely: everything **device-agnostic** is built and tested against the CPU reference of the GPU route; the CUDA kernel and the runner — the two things that need hardware and money — are **not** faked. Contract first, as always: [the determinism contract's device edition](DETERMINISM-CONTRACT.md) (§8–§12) was written before any of it.

### 1. The route is invisible to the answer — selection-identity, not score-identity

A GPU sums in a different order than a CPU, so its scores differ in the last bits; a GPU **cannot** be bit-identical, and S7 does not pretend it can. So the contract weakens to what is achievable and sufficient: **scores may differ within a documented tolerance; the returned event ids and order may not** ([D-050](DECISIONS.md)). This rests on [charter C-4](DECISIONS.md) already being law — ties break on `event_id`, not on a score's last bit.

The `GpuReference` route is the **CPU-executed definition** the real CUDA kernel will have to match — the scalar-kernel-for-SIMD pattern, one substrate up. It reduces in a genuinely different (pairwise) order, so the selection-identity gate exercises a *real* score difference; the suite refuses to pass if the two routes ever produce bit-identical scores, because then the tolerance was never tested.

### 2. A cursor survives a route flip — the gate that forced the right design

Directive 3's gate — paginate while the route flips between pages — failed the obvious implementation: the cursor stores a **score** for keyset pagination, scores differ by route, and the page boundary breaks. The fix is the one the cursor already uses for the snapshot: **pin the route** ([D-052](DECISIONS.md)). A paginated query is one logical query; its route is fixed at page 1 and carried in the cursor, so flipping the external route between pages is invisible and the pages tile the answer exactly — no duplicate, no gap.

### 3. A device fault degrades to CPU; admission is per tenant

A CUDA error at any phase — upload, kernel, selection, download — **degrades to the CPU path with a logged event**, never a failed query or a wrong answer ([D-051](DECISIONS.md)). The rerank fetches every candidate's vector first and reranks second, so a fault is a pure recompute. And device-memory admission is **per tenant**: tenant A's OOM cannot fail tenant B, the ingest path's starvation isolation now on the device. Both are fully tested against the reference's injected faults and a fake device budget.

### 4. fp16 rerank — the first negotiated accuracy contract

Storing rerank vectors in fp16 halves the exact-tier storage bill, at the price of approximation — a [D-003](DECISIONS.md) event, not a kernel detail ([D-049](DECISIONS.md)). fp32-exact stays the only default; fp16 is opt-in behind `encoding_id=2 / accuracy_contract_id=2`, and a build that does not implement the pair **refuses the part** rather than guessing. Contract text, evidence, and the unknown-encoding fixture were updated in one change, setting the precedent.

**The honest finding: fp16 is not strict-order-stable, and cannot be** — any lossy encoding reorders rows within its rounding error. So the guarantee is the achievable one, and it is *derived*, not asserted: with the tolerance set above **twice** the worst per-score gap, fp16 can never invert a pair separated by more than the tolerance. Measured worst gap `4.6e-4` → floor `9.2e-4` → committed `2e-3` with headroom. The receipt proves selection stability at that tolerance; the CI job re-checks both conditions.

### 5. Charter C-6 — receipts re-derive at sprint end

S6's block-size panic (the manifest had grown out from under the receipt) is now a standing rule: **every C-1 receipt re-derives at the end of any sprint that materially changes the engine, the format, or a corpus** ([C-6](DECISIONS.md)). It subsumes the corpus- and generation-conditional obligations into one checklist line. This sprint's re-run reconfirmed every recall constant (the device path is CPU today, so nothing moved) and added the fp16 receipt.

### What is deliberately NOT here

- **The CUDA kernel.** Declared behind the `cuda` feature, *not compiled in CI* — writing untestable FFI is the faked completeness the project refuses. `Route::Cuda` is off by default.
- **The GPU runner.** Provisioning-as-code in [`infra/gpu-runner/`](../infra/gpu-runner/) — real Terraform + cloud-init, one `terraform apply` from a live runner — but **not applied**, because there are no credentials. When it lands, the CUDA kernels graduate through the same selection-identity gate the reference passes today.
- **The crossover thresholds.** Placeholders marked device-conditional and un-derived; deriving them requires measuring a GPU (charter C-6, filed).
- **The 4-bit-shuffle CPU kernel** — the genuine speedup S6's NEON finding pointed at — filed as [issue #4](https://github.com/Bobcatsfan33/PrismDB/issues/4), not built. One substrate per sprint (directive 7).

---

## S8 — complete

**Proof:** [CI run #29434139708](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29434139708) — all 25 jobs green on `main`, including the S8 plan-invariance gate (every strategy byte-identical on golden + layout-variant + boundary-tie, plus paginate-under-plan-flip), the worst-cell regret gate (≤ 15% in every selectivity cell), the selectivity calibration harness, the Flight-SQL same-door parity + bounded-decode fuzz, and the full suite in debug **and** release. 298 tests.

**Gate:** *full SQL semantics — nulls, ties, ordering, threshold-vs-top-k, generation selection — and a cost-based optimizer that beats any fixed plan without ever changing an answer.* Contract first: [query contract §9–§14](QUERY-CONTRACT.md) was written before the code.

### 1. Plan-invariance — the sprint's central gate

Three physical strategies — interleaved, scalar-first, semantic-first — for **one logical query** ([D-054](DECISIONS.md)). They cost differently; they answer **byte-identically**, because all three offer the *identical candidate set* (the top-`candidates` predicate-passing rows by PQ distance) to the *identical* top-k, differing only in when the predicate runs. The gate forces each strategy on golden + layout-variant + boundary-tie corpora; a companion test proves the *work* genuinely diverges (scalar-first computes far fewer distances, semantic-first far fewer predicate evals). **Because the plan changes no score, a cursor need not pin it** — and the gate proves it by paginating while flipping the plan between pages.

### 2. The optimizer — receipts, and worst-cell regret

The plan choice turns on a **crude sampled selectivity** estimate (directive 7: choose among three strategies well, do not research cardinality estimation), and the metric is **worst-cell regret ≤ 15%, not average** ([D-055](DECISIONS.md)) — the chosen plan within the bound of the best fixed plan in *every* cell of the selectivity matrix, measured as a deterministic counter proxy. **It caught a real cost-model bug immediately:** semantic-first's predicate saving materializes only once the heap fills (~`cap/selectivity` rows), and modelling `cap` instead gave 76% regret at low selectivity — the worst-cell metric surfaced where an average would have buried it.

The cost coefficients are policy informed by a microbench ([`cost-model.json`](../testing/evidence/cost-model.json)), with an honest surprise: a real interpreted `predicate::eval` costs *more* than a SIMD-batched distance, which is why semantic-first often wins. The **GPU axis stays inert** — `usize::MAX` threshold, `gpu_available()` false — so the planner never steers on a coefficient with no evidence (directive 2).

### 3. Query semantics, stated to teach

Nulls (absent, two-valued, not `NULL`-propagating), ties (C-4 at the SQL level), threshold × top-k (threshold first, on the exact score), and generation selection — with a **cross-space error written to teach**: it names both spaces, explains that a cosine of 0.8 in one is not a cosine of 0.8 in the other, and offers the three fixes ([D-056](DECISIONS.md)).

### 4. EXPLAIN + calibration, and a third door

EXPLAIN carries estimates *and* actuals for the four controls plus the chosen route/plan and the reason; a calibration harness tracks estimate-vs-actual selectivity error in CI so drift is a visible number ([D-057](DECISIONS.md)). And the **Flight SQL door is the same door** — tenant injected below it, byte-identical counters across direct/SQL/Flight, bounded decode (every length capped and named, garbage never panics). **Its server-side query path ships; the Arrow IPC/gRPC transport does not** — that needs the arrow ecosystem the serde-only charter excludes and a network server the roadmap defers to S14. The same-door property, which is what matters for correctness, is proven three-way today.

### Findings

- **The regret gate caught a cost-model bug** (semantic-first heap-fill dynamics) that an average-regret metric would have hidden.
- **A real predicate eval is costlier than a SIMD distance** — the honest reason "distance-first" wins, documented rather than assumed.
- **The calibration harness caught its own subtlety** — `actual_selectivity` is strategy-dependent, so it measures the true rate from a forced-interleaved run.

---

## S9 — semantics complete; the 100M<10s gate is measured, not claimed

**Proof:** [CI run #29464845219](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29464845219) — all 26 jobs green on `main`, including the new S9 `cluster` gate (semantic_cluster determinism across layouts + plan/route flips, ARI ≥ 0.8 on the frozen labeled corpus incl. the adversarial shapes, noise asserted low-confidence, k-cap refusal, C-4 exemplars, merge-order invariance; injected-novelty precision/recall on the tail; cross-space refusal), the C-1 constant ledger, and the full suite in debug **and** release.

**Gate:** *semantic `GROUP BY` over a 100M-row filtered set < 10s single node; ARI ≥ 0.8 vs oracle on labeled synthetics; partial-state merge property tests; injected-novelty precision/recall ≥ 0.9.* Contract first: [determinism contract §13–§15](DETERMINISM-CONTRACT.md), [query contract §15–§18](QUERY-CONTRACT.md), and charter **[C-7](DECISIONS.md)** were written before the clustering code.

### 1. The randomized aggregate is a function of the data — the central gate

`semantic_cluster` clusters an arbitrarily large *filtered set* by meaning, and a randomized algorithm is where determinism contracts usually die. **[C-7](DECISIONS.md)** closes the two new leaks: the PRNG is seeded from **content** — `SHA-256(sorted event_ids ‖ k ‖ generation)`, never a clock — and rows are consumed in **logical (`event_id`) order**, never scan order. The gate asserts **identical clusters, exemplars, and per-cluster aggregates** across two physical layouts *and* across forced plan-flips and route-flips (the S7/S8 controls turned on the aggregate). Exemplars are a **C-4 selection on the exact score** (a mislabeled cluster is a wrong answer a human reads), proven on the boundary-tie logic. Partial states **merge in canonical (shard-id) order**, and a property test asserts the *invariance* — the same partials in a scrambled order produce byte-identical aggregates — not merely correctness ([D-058](DECISIONS.md)/[D-061](DECISIONS.md)).

### 2. ARI ≥ 0.8 against ground truth, including the shapes a demo would hide

The oracle is **ground-truth labels**, not a reference clusterer — labeled synthetics are the exact answer sklearn would only estimate, and the charter forbids the dependency anyway ([D-059](DECISIONS.md)). The frozen corpus ([`testing/cluster/v1`](../testing/cluster/v1), C-2) carries the adversarial shapes: **balanced** (ARI 0.91), **Zipf-skewed sizes** (0.92), **touching boundaries** (0.83), and **uniform noise**, where the honest answer is asserted **low-confidence** (`quality = 1 − inertia_k/inertia_1` below the floor) rather than dressed up as `k` confident groups. The aggregate is **bounded before it runs** ([D-060](DECISIONS.md)): `k` over `MAX_SEMANTIC_K` and a working set over the state budget are **named refusals**, never a silent clamp or an OOM.

### 3. Novelty and drift, accurate on the tail

`NOVELTY … AGAINST` reuses the S5 drift baseline; `SEMANTIC_DIFF` is the aggregate asked a comparative question ([D-063](DECISIONS.md)). The injected-novelty benchmark holds **precision 0.96 and recall ≥ 0.9 on the worst seeded class** (the S1 tail lesson — a mean would hide the one novelty kind that gets missed), and a `NOVELTY` scoring rows against a baseline in another embedding space is the **invariant-9 refusal**, proven across a real v1→v2 re-embed.

### 4. The 100M<10s gate is measured, not claimed — and the SQL surface is deferred

**The honest wall, in S7's shape** ([D-062](DECISIONS.md)). The quality params that clear the ARI floor (5 restarts × 15 epochs) make the fit ~75 passes; at the measured **~10⁴ rows/s single-core** ([`semantic-cluster.json`](../testing/evidence/semantic-cluster.json)) that projects to ~10⁴ s for 100M — a ~1000× gap — so the gate is **not claimed**. What is built is the mechanism the target needs (bounded-state streaming fit, mergeable partials, deterministic answer); what is filed is the scale profile it does not tune this sprint (single-restart fit, PQ-code ADC assignment, SIMD, multi-core, streaming PQ-code fit). And the SQL *keyword* grammar (`GROUP BY semantic_cluster(…)`, `NOVELTY … AGAINST`, `SEMANTIC_DIFF`) is deferred like S8's Flight transport: the semantics ship first at the engine level and are gated, the surface that spells them follows ([D-063](DECISIONS.md)).

### Findings

- **The clustering oracle is better than the one PRISM named** — ground-truth labels are exact where sklearn would approximate, and they cost no dependency.
- **A single k-means++ draw is a lottery** on this corpus (ARI 0.76); restarts make the fit a function of the data, the same lesson D-036 taught the codebook.
- **A few synthetic novel classes collide into the baseline's hash buckets** (a 64-dim hash-embedder artifact) and are not actually novel; the benchmark injects the genuinely-far classes and records the caveat, because labelling a colliding class "novel" would benchmark the embedder, not the alarm.
- **The 100M gate needs a scale profile, not a faster core** — the shipped quality params are ~1000× too slow for it, and no committed number supports a 10s claim, so none is made.

---

## S10 — complete

**Proof:** [CI run #29505875380](https://github.com/Bobcatsfan33/PrismDB/actions/runs/29505875380) — all 28 jobs green on `main`, including the new ENOSPC fault matrix (disk filled mid-part-write / mid-snapshot / mid-CURRENT, each old-or-new and recovering), the size-tiered scheduler (bounded steady-state part count + debt, answers unchanged, fair admission), the reader-lease gate (invariant 6 by construction, crashed-reader expiry), the tombstone gate, and the accelerated soak.

**Gate:** *sustained ingest → steady part count and merge debt; random kills during query/merge/delete never expose partial results; a re-embed migration pauses/resumes/rolls back.* Contract first: [docs/MERGE-CONTRACT.md](MERGE-CONTRACT.md) was written before the scheduler.

### 1. ENOSPC is a first-class fault — the lesson the build host taught

This clause exists because the storage engine's own build host ran out of disk during the project. `PrismError::OutOfSpace` is a **named** condition (a real errno 28 maps to it, so a genuine full disk degrades exactly like an injected one), merge admission refuses a merge **before it starts** unless the projected output plus a safety margin fits free space (a named `deferred`, store untouched, recovers unaided), and a new injectable out-of-space fault fills the disk **mid-part-write, mid-snapshot-commit, and mid-CURRENT-swap** — each degrades old-or-new-never-hybrid and recovers when space returns. The temp+rename discipline made this safe by construction; the tests make it proven.

### 2. The merge scheduler — size-tiered, explainable, bounded, fair

Merge is now **size-tiered** ([`scheduler.rs`](../crates/prism-engine/src/scheduler.rs)): parts bucket by size, a tier at the fan-out is merged and graduates one tier up, so part count reaches a **steady state** and write amplification stays bounded — proven by a sustained-ingest test whose part count and merge debt stay bounded across 40 cycles instead of growing. The decision is **explainable, not deterministic** ([merge contract §2](MERGE-CONTRACT.md)): every op records its tier, parts, debt, and budgets spent, enough to reproduce *why* — but the scheduler is not coupled to a global order, because a merge changes no answer (C-4/C-5), so there is nothing for merge-determinism to protect. Admission is **round-robin across buckets**, so a saturating tenant cannot starve a quiet one's merges (bounded delay, property-tested). Every constant is a registered C-1 policy with a rationale.

### 3. Reader leases and GC grace, by construction

There is **one** lifecycle-timing constant, `LEASE_TTL_MS`; GC grace and the reclaim horizon are `const fn`s of it, so `grace < lease` can never arise and orphan a live reader ([merge contract §5](MERGE-CONTRACT.md), invariant 6 by construction). A reader within its lease survives GC even at `retain = 1`; a crashed/expired reader's snapshot is reclaimed and its stale cursor returns the explicit expired-snapshot error — proven both halves.

### 4. Deletes are tombstones, and when a deleted row leaves a baseline is decided

A delete writes a **tombstone** (one atomic catalog commit): logical at once (queries filter it), physical at merge (reconciled away, a full merge clears the set), idempotent. And the directive's demanded decision is written down ([D-064](DECISIONS.md)): a deleted row leaves the drift baselines at the **next scheduled baseline snapshot**, not at merge time — three named clocks (query answers at tombstone commit, baselines at the next recompute, bytes at merge), none secretly driving another. Frozen receipt corpora are never touched.

### 5. The gate is a soak

The accelerated soak runs sustained ingest **and** queries **and** deletes **and** a re-embed migration together, with the tiered scheduler compacting throughout, and asserts a **steady-state part count**, **bounded merge debt**, and a **canary exact answer byte-identical to cycle 1** — the S8/v1 recall-stability discipline, now with mutations underneath it. The full-length soak is nightly; kill-injection at merge boundaries is the fault matrix's half (an abort cannot run in-process), and the ENOSPC matrix is new this sprint.

### Findings

- **A database must beat the standard its build environment failed** — so ENOSPC became a first-class, injectable, tested fault, not a generic I/O error.
- **Explainability, not determinism, is the right bar for a scheduler** — answers are layout-invariant already, so coupling the scheduler to a global order would only make it depend on things it must not see.
- **One constant, not two that can drift** — deriving GC grace from the lease makes invariant 6 hold by construction instead of by a reviewer noticing `grace < lease`.
- **Deletion has three clocks** — query, baseline, and bytes — and naming each (D-064) is what keeps a compliance-relevant fact from silently depending on merge cadence.
- **Remaining honest gap:** per-tenant write amplification is not yet a broken-out counter (the store-wide `write_amplification` is; per-tenant accounting rides on the fairness round-robin and is the next increment), and the CLI has no `delete` subcommand yet (the engine API is gated).
