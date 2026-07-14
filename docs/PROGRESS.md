# Progress

Sprint gates from [PRISM.md](PRISM.md) Part IV. **A sprint is done when its gate is reproducible in CI — not when the code merges.** Each completed gate links the run that proves it.

| | Sprint | Gate | Status |
|---|---|---|---|
| **S0** | Executable reference slice + oracle harness | clean checkout passes CI; baselines checked in; README quickstart runs | ✅ **complete** |
| **S1** | Part format + recovery hardening | 10,000 randomized kill/reopen runs → old-or-new, never hybrid; all compat fixtures open, all corrupt fixtures rejected specifically; no untrusted length allocates unbounded | ✅ **complete** |
| **S2** | Production ingestion + OTel GenAI schema | replaying acknowledged input loses no rows; offsets never advance pre-publication; one tenant cannot starve another | ✅ **complete** |
| **S3** | Minimal SQL + scalar analytics | scalar-subset parity against an oracle; hybrid smoke queries identical through SQL | ✅ **complete** |
| S4 | Hybrid partitioning + typed columns + data skipping | selective benchmarks read only eligible partitions; cross-tenant reads impossible even with malformed metadata | ⬜ **next** |
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

## S4 — next

**Hybrid partitioning + typed columns + data skipping.** Outer partitions (tenant-bucket × time × generation); typed scalar columns with null maps and dictionary/delta compression; block min/max, low-cardinality sets, bounded Blooms.

**Gate:** selective benchmarks read only eligible partitions/blocks; recall within tolerance of S0; **cross-tenant reads impossible even with malformed metadata or a poisoned cache**.

**Carry into S4:**
- **Typed columns and partition metadata land as *flagged extensions on v3*, not v4** ([D-020](DECISIONS.md)). The feature bits are already reserved and already refused by this build.
- **Attribute promotion is [issue #2](https://github.com/Bobcatsfan33/PrismDB/issues/2)** — and it belongs here, because promotion only pays once the block-skipping machinery it depends on exists. Its acceptance criteria demand the promotion decision itself be *derived* from committed evidence (cardinality, selectivity, query frequency) rather than a hand-picked list of key names — charter C-1 applies.
- **Tenant isolation becomes structural.** Today it is a filter fused into the scan plus a pruning predicate — strong, and tested against 8,000 fuzzed statements, but still a *check*. S4's outer partitioning makes a cross-tenant read *physically impossible*, which is a different and better kind of guarantee. The S4 gate says "even with malformed metadata", so the fuzzing moves down a layer: poison the manifest, not the SQL.
