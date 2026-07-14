# Decisions

Judgment calls made where [PRISM.md](PRISM.md) is silent. The rule: **where the contract has an answer, follow it; where it does not, choose the boring option and write it down here.** A decision recorded badly is worse than no decision, so each entry says what was chosen, what was rejected, and what would make us revisit it.

---

# Charter amendments

These extend [PRISM.md](PRISM.md) Part II §7 and carry the same weight. A change that violates one is rejected however good it is otherwise.

## C-1 — No tuned constant without committed evidence and a test that binds them
**S2. Architect's standing rule.** Generalizes the `DEFAULT_NPROBE` pattern from S1 ([D-017](#d-017--the-default-nprobe-is-derived-from-the-tail-and-carries-a-receipt)).

PRISM.md Part I §5.3 already forbids magic constants in *defaults and docs*. This makes it a mechanism instead of an intention:

> **Every tuned constant must be pinned to committed benchmark evidence, with a test asserting the constant still matches that evidence.**

The machinery:

- `crates/prism-engine/src/tuning.rs` enumerates every constant that steers behaviour, and classifies each one.
- `testing/evidence/registry.json` is the committed ledger: name, value, kind, and — for a tuned constant — the evidence file, the exact key inside it that justifies the value, and the **rule** by which that key was chosen.
- `every_tuned_constant_matches_its_committed_evidence` asserts, **in both directions**, that the registry and the code agree; that every `tuned` constant's evidence file exists; and that the value in the code equals the value the evidence chose. A constant cannot drift from its receipt, and a new constant cannot be added without landing in the ledger.

**Two kinds, and the distinction is load-bearing:**

- **`tuned`** — the value was *derived from measurement*, and a different measurement would have produced a different value. It owes evidence. `DEFAULT_NPROBE` and `BLOCK_SIZE` are tuned.
- **`policy`** — the value is a *deliberate choice about behaviour*, not an empirical optimum. A cap of 64 attribute keys is not "the measured best number of attribute keys"; it is a decision about what we are willing to accept. It owes a **rationale**, pointed at a section of a document, and the test enforces that the pointer resolves.

The distinction exists to be *abused-proof*: without it, every inconvenient constant gets reclassified as policy to escape the evidence requirement. So `policy` still has to point at prose that argues for it, and prose is reviewable.

**The first thing this rule caught was our own.** `BLOCK_SIZE = 64 KiB` was picked in S1 because 64 KiB is what people pick. Under C-1 it had to be derived or it had to go — see [D-019](#d-019--block_size-is-64-kib-because-it-was-measured-not-because-it-is-traditional).

## C-2 — Golden corpora are frozen, immutable, versioned artifacts
**S3. Architect's standing rule, arising from [D-023](#d-023--the-corpus-generator-draws-s2-fields-from-a-separate-rng-stream).**

> **A drift check compares committed bytes. It never compares regenerated output against regenerated output.**

In S2 a change to the corpus *generator* silently changed the corpus, and `make-fixtures.sh` cheerfully regenerated both the corpus **and** its expected answers — so the drift check went on passing **by construction while testing nothing at all**. The near-miss was caught by luck. C-2 makes it impossible by construction instead:

- `testing/golden/` holds **versioned** corpora (`v1/`, `v2/`, …) and a `MANIFEST.json` naming the `current` one and carrying a **SHA-256 of every committed file**.
- **`make-fixtures.sh` no longer generates the corpus. It verifies it**, against those checksums, and fails if a frozen artifact has been modified in place.
- A corpus change is a **new version**, created by `scripts/new-golden-corpus.sh`, which **refuses to overwrite an existing one**, demands a `--reason` a reviewer can evaluate, and prints the diff against the current corpus for review.
- **Predecessors are retained forever.** Every receipt — an nprobe sweep, a block-size derivation, a recall number in a release note — names the corpus version it was measured against. A receipt that points at a corpus which no longer exists is not a receipt.
- Flipping `current` to a new version is a **separate, deliberate edit**, and it invalidates every receipt derived from the old corpus, so those must be re-derived in the same change.

`testing/golden/v1/corpus.tsv` is byte-identical to the corpus S0 committed. It is only *because* it never moved that S1's recall-tail finding — and every constant derived since — still means anything.

## C-3 — A sweep declares its policy bounds, and a boundary optimum is an artifact
**S4. Architect's standing rule.** Formalizes the pattern that has now appeared three times.

Every C-1 sweep must:

1. **Declare its policy bounds explicitly**, in the receipt, each with a **written rationale for why measurement cannot see it.** A tuned objective almost always sits inside a constraint the numbers do not contain. Three examples, all real:
   - `BLOCK_SIZE`: minimise bytes read — *subject to* the manifest block directory staying under 4 bytes/row, because it is read in full on every part open and a billion-row part would otherwise carry a 16 GB directory. The byte count cannot see that; it only sees a 2,000-row corpus where the manifest term is invisible.
   - `DEFAULT_RERANK`: smallest width clearing the recall floors — *subject to* `MIN_PAGEABLE_ROWS = 50`, because the paginated result set **is** the rerank survivor set and a width of 10 makes the cursor decorative.
   - `DEFAULT_NPROBE`: chosen on the **tail**, not the mean — because a mean of 0.904 with five queries returning nothing is not a 90% system.

2. **Treat an optimum on the boundary of its grid as presumptively an artifact.** A sweep whose winner is its own smallest or largest candidate has not found an optimum; it has hit a wall. **Expand the grid, or find the missing constraint, before committing the receipt.** Both block-size sweeps hit this: the first collapsed onto 4 KiB (the smallest candidate offered), and extending the range downward showed the true bottom was 512 B — at which point the *missing constraint* was the manifest budget, not a wider grid. The evidence tests now assert the chosen value is strictly inside the swept range.

3. **Record the golden-corpus version that produced it.** A receipt that does not name its corpus is not interpretable once the corpus moves — and under C-2 corpora are versioned precisely so that they can move without invalidating history.

4. **Mark constants derived on a hash-embedder corpus `corpus_conditional: true`.** The deterministic hash embedder exists so that tests and baselines are reproducible with no weights and no network. It is the right tool for that and the **wrong corpus to tune an index on**: its motifs are unusually well-separated, which is exactly why the `(candidates × rerank)` sweep found the recall floors *did not bind at all*. Every constant so marked is a constant we already know may move.

**[Issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3) files the work**: create a real-embedding golden corpus (small open model, CPU, checksum-pinned, frozen per C-2) and re-derive every C-1 sweep against it. **Not deferred to S13** — S13 is the production GPU model plane, which is not a prerequisite for running one small model once, offline, and committing its output. Waiting means every constant derived between now and then is derived on a corpus we already know is unrepresentative.

**Extended in S5 (directive 3):** receipts are **generation-conditional** as well as corpus-conditional. A new codebook generation changes the PQ geometry, so `nprobe`, candidate width and rerank width may not transfer across one. Every tuned entry now carries `generation_conditional: true`, and re-sweeping on a new generation is the same standing obligation as re-sweeping on a real-embedding corpus.

**This is not hypothetical.** S5's C-4 fix to the training sample changed the codebook, and `DEFAULT_NPROBE = 4` immediately stopped clearing its own floor (p1 recall 0.70 against a floor of 0.80). The constant had to be re-derived against the new geometry. A receipt that describes an engine which no longer exists is not evidence; it is decoration.

## C-4 — A bounded selection breaks ties on identity, never on position
**S5. Architect's standing rule, promoted from [D-033](DECISIONS.md).**

> **Any bounded or truncating selection breaks ties on logical row identity (`event_id`), never on physical position.**

D-033 was one instance: the bounded candidate heap broke score ties on `(part, row)`, so repartitioning the store changed which tied rows were *allowed to be answers*, and recall fell from 1.00 to 0.60 while probing harder did nothing. The rule generalizes the lesson, because the defect is a **class**, not a bug: anywhere the engine keeps *some* of a set and discards the rest, the tie-break decides what the store is allowed to say — and if that tie-break is an address, the store is answering questions about its own layout.

Three obligations, all permanent:

1. **Audit every bounded structure**, and bind each to a **layout-variant test**. The S5 audit swept the candidate heap, the rerank truncation, semantic-grouping exemplars and membership, merge duplicate reconciliation, idempotency-index eviction, the recall report's worst-query list, and codebook training. It found **two live violations** ([D-034](DECISIONS.md), [D-035](DECISIONS.md)) in code that had passed every test in the suite.

2. **Layout-variant golden fixtures, as a permanent gate.** One **frozen logical corpus** (C-2), materialized under **four** physical layouts — different time windows, different ingest batchings, before and after a merge, one part to fifty-five. Every golden query must answer **byte-identically** across all of them. `crates/prism-cli/tests/layout.rs`.

3. **The gate is permanent.** It does not get retired when the current violations are fixed, because the class is what recurs. A merge runs; a window is retuned; a batch size doubles. None of those is a decision about *answers* — and every one of them silently was.

**Why a layout-variant test and not a code review:** each of these sites was individually self-consistent and deterministic. `(part, row)` *is* a deterministic tie-break, and the comment on the candidate heap said exactly that, and it was true, and it was the bug. Determinism in a layout is not the same property as being a function of the data, and only running the same data through two layouts can tell them apart.

---

## D-001 — `docs/PRISM.md` is the canonical v2 text — *verified*
**S0, closed in S1.** The build instruction said to copy `06-prism-overview-and-roadmap-v2.md` from the working directory; no such file existed anywhere on the machine, so `docs/PRISM.md` was written from the v2 text supplied inline, and this entry flagged the risk.

**The canonical file has since been located** (in the architect's outputs directory) and diffed against `docs/PRISM.md`: **byte-identical, 29,080 bytes, zero differences.** The contract is verified rather than assumed. Nothing further to revisit.

## D-002 — Two dependencies: `serde` and `serde_json`. Nothing else.
**S0.** The charter says dependency-light. Manifests, catalog snapshots, generation records and CLI output are all JSON, and hand-rolling a JSON parser would be strictly worse than using the standard one — more code, more bugs, no benefit.

Everything else is implemented in-tree, on purpose:
- **CRC-32 and SHA-256** (`prism-types::hash`) — invariant 8 (content-addressed codebooks) and invariant 10 (checksums cover stored bytes end to end) are load-bearing. They are verified against published test vectors, so this is not "trust me", it is "here is the NIST vector".
- **The PRNG** (`prism-types::rng`) — every stochastic step must be reproducible from a seed or the baselines and the recall contract are not reproducible either.
- **Argument parsing** — a flag parser would have been the largest thing in the dependency tree, for ten subcommands.

**Rejected:** `clap`, `rand`, `sha2`, `crc32fast`, `thiserror`, `anyhow`.
**Revisit if:** a hand-rolled primitive shows up as a hotspot in a profile (then swap the *implementation*, keeping the tests), or the CLI grows a surface that genuinely needs a parser.

## D-003 — The rerank tier is full float32, in a separate file
**S0. Superseded by D-003-resolved.** PRISM.md Part I §5.2 lists four options for the exact-rerank tier (full vectors cold, fp16 with an accuracy contract, re-embed on demand, residual quantization) and deliberately does not choose. S0 had to store *something*, and the choice was going to freeze into the part format.

Chosen: **full float32, in its own column file, never on the scan path.** The boring option, and the only one with no accuracy contract to negotiate: the re-rank is exact because the vector is exact. Keeping it in a separate file rather than interleaved is what makes the tier separation physical — the scan reads `pq.codes` and never opens the rerank column, and `bench` reports the two byte counts separately, always.

This was flagged as **the biggest open cost question in the project**: full float32 is ~3.07 PB per trillion vectors against ~96 TB for the codes — a 32× multiple that dominates the storage bill, and "cost per billion events retained and queryable" is one of the three numbers the company watches.

## D-003-resolved — The encoding stays float32; the *format* stops assuming it
**S1. Architect's decision.** Keep float32-cold as the only implemented encoding, and make the format stop hard-coding that assumption.

Every v2 part manifest now carries a **versioned rerank-tier descriptor**: `{ encoding_id, accuracy_contract_id }`. The reader **dispatches** on it (`RerankDescriptor::decode_vector`, `bytes_per_vector`) rather than assuming 4 bytes per dimension anywhere. Centroid marks size their rerank byte ranges from the declared encoding. An id this build does not implement is **refused at open time** with a specific error — never decoded into plausible-looking numbers. `testing/compat/corrupt-v2/unknown-rerank-encoding/` is the fixture that proves it, and it is the fixture that will matter on the day fp16 ships: it is what stops an older binary from silently reading fp16 bytes as float32.

The consequence is the point: **changing the encoding later is a generation migration, never a format break.** Parts are rewritten by a merge into a new encoding, old parts remain readable, and rollback stays a catalog write. The column file is named `rerank.vec`, not `vectors.f32` — a name that hard-codes `f32` becomes a lie the day it is not.

The cost question is not answered, and it is not supposed to be. It is now *changeable* without breaking anyone, and *measurable* (`rerank_tier_multiple` in `baselines.json`, currently exactly 32.0×).
**Revisit at:** S11, when the two-tier cost becomes a product surface and there is real evidence about what accuracy the rerank tier actually needs.

## D-004 — Whole-file CRC now; block framing at S1
**S0. Done in S1.** Each v1 column file carried one CRC-32 over its whole contents, so a single flipped byte condemned an entire column.

v2 column files are **framed into 64 KiB checksummed blocks**. Damage now localizes to one block and the error names it (`column 'body.data' block 7 failed checksum: expected …, computed …, logical bytes 458752..524288`). A ranged read fetches only the blocks that overlap the range, which is what makes "we read only the ranges we selected" a fact about the syscalls rather than a claim. The block size is a constant, not a per-part field, because a variable block size buys nothing today and would be one more untrusted number to validate.

## D-014 — v2 is written; v1 is still read
**S1.** The alternative was to declare v1 unreleased (it shipped for one day, to nobody) and regenerate the fixtures at v2 — cheaper, and it would have made `testing/compat/` a directory of bytes that only ever agreed with the code that wrote them.

Instead: **`PartWriter` only writes v2. `PartReader` opens both.** A part announces its format by which manifest file it carries (`manifest.bin` vs `manifest.json`) — the reader never guesses, and a directory with neither is not a part. `legacy_v1.rs` decodes the old JSON manifest into the same in-memory structure, synthesizing the rerank descriptor that v1 implicitly had (float32/exact, because it had no choice). Every reader above that line stops caring which format it came from.

Why it is worth the ~150 lines: the compatibility promise is kept by *keeping it*. It also exercises the exact version-dispatch discipline that D-003-resolved requires for the rerank encoding — the same machinery, proven twice.

**And v1 parts are migrated forward by a merge**, not by a rewrite-in-place: a legacy part is on its own sufficient reason for a merge to run, even when there is nothing to compact. This was a real gap found by a failing test — a single-part v1 store previously had *no way to reach v2 at all*, because merge short-circuited on `parts < 2`. The v1 bytes are never touched; a new v2 part is written and the catalog swapped, so rollback still works.

## D-015 — `fsck` is offline, exhaustive, and does not need a catalog
**S1.** The contract asks for an "offline format validator". `prism fsck` takes a directory of bytes and answers one question: *is this a part, and is it intact?* No engine, no catalog, no generation, no store. An operator holding a suspicious object out of a backup or an object store must be able to condemn it — or clear it — without standing a database up first.

It reports **every** finding rather than dying on the first, because the blast radius is what an operator needs at 3am, not the first casualty. It also checks two things a checksum cannot: that every stored vector is **unit-norm** (the engine assumes this and never re-checks it, so a part that breaks it produces *silently wrong scores* rather than errors — the worst possible failure), and that rows really are in `(centroid, event_time, event_id)` order (if they are not, a centroid is not a contiguous byte range and probing returns the wrong rows).

## D-016 — Truncation must name itself
**S1.** Found by a fixture. A truncated column file produced `io error: failed to fill whole buffer` — a generic read failure that tells an operator nothing. The S1 gate says corrupt fixtures must be rejected *with specific errors*; a raw `ErrorKind::UnexpectedEof` is not one.

The reader now stats the column file and checks each block's byte range against its actual length **before** reading, so truncation is reported as `column 'rerank_vectors' is truncated: block 3 needs bytes 196608..262144 of 'rerank.vec', but the file is only 131072 bytes`.

## D-005 — Oversized bodies are dead-lettered; embedding input is truncated
**S0.** Two different limits, deliberately:
- A body over **1 MiB** fails admission and is **dead-lettered**. It is not truncated and stored, because a stored event whose body is silently half-missing is a lie that no one will ever catch.
- A body under that limit but over **32 KiB** is stored *in full*, and only the first 32 KiB is fed to the embedder. Embedding cost is bounded; the data is not damaged.

**Revisit if:** a real corpus shows meaningful signal past 32 KiB, or the 1 MiB admission limit rejects legitimate traffic.

## D-006 — An empty body is an error, not a null vector
**S0.** An event whose body has no tokens cannot be embedded — the vector would have zero norm, which is not a point on the sphere and not a thing cosine is defined for. The contract (Part III §10) is unambiguous: *never silently store an event without its requested semantic columns.* So it is dead-lettered, visibly, with a reason.
**Rejected:** storing a zero vector, storing a null, dropping the row. All three produce an event that will never match any semantic query, for reasons nobody can reconstruct six months later.

## D-007 — A query spanning two embedding spaces is refused
**S0.** Invariant 9 forbids merging scores across embedding spaces without a validated bridge. Two codebook *generations* in the same space are fine — each gets its own ADC table, and they merge at exact-score time, where both agree on what a vector means. Two different *models* are not.

Given a store holding both, there were three options: merge the scores anyway (violates the invariant), silently search only one space (silently omits data — the thing the contract forbids most), or **refuse and make the caller name the space with `--space model:version`**. We refuse. The error names both spaces and says how to proceed. A re-embed migration is what removes the mixed state.
**Revisit at:** S5, when the generation lifecycle lands and a bridge policy may become definable.

## D-008 — Ranking ties break on `event_id`, never on physical position
**S0.** Found by a failing test, not by design: `merge_preserves_results` failed because merging returned *the same ten events in a different order*. Real telemetry repeats bodies verbatim, so exact-score ties are common, and the ranker had been breaking them on `(part, row)` — physical position. That meant the same query returned a different order after a merge moved rows between parts, and it meant the scan path and the exact oracle could disagree on tied results, which would have quietly polluted every recall measurement.

Order must be a function of the data, not of the layout. Ties now break on `event_id`, in both the scan path and the oracle.
**Consequence:** the ranker fetches `event_id` for re-rank survivors before sorting. One extra column read on a bounded set — cheap, and the price of an ordering that means something.
**Note:** full null/tie/ordering/pagination semantics are an S8 deliverable. This is the floor, not the finished contract.

## D-009 — `verify` decodes structure, not just checksums
**S0.** Found by the `bad-offsets` compat fixture. A string-offset array can be perfectly checksum-valid and still claim a row begins 1 TiB into a 4 KiB blob: a CRC proves the bytes are the bytes we wrote, not that they *mean* anything. The first `verify` implementation checked checksums only, and accepted the fixture.

`PartReader::verify` now checksums **and** fully decodes, which forces every untrusted length through the bounds checks in `decode_strings`. `open` deliberately still does neither — paying a full CRC and a full decode on every part at query time would make pruning pointless. Integrity auditing is an explicit operation.

## D-010 — Bootstrap codebooks are trained on a reservoir sample, not the first N rows
**S0.** PRISM.md S5 warns specifically against training codebooks on the first batch. The first ingest into an empty store has no other data to train on, so it trains on a reservoir sample *of that batch* — spread across the whole batch rather than its head — and the resulting generation records `trained_from` so its provenance is visible. `reembed` retrains over a reservoir sample of the entire store.
**Consequence:** a store whose first batch is unrepresentative has skewed centroids until it is re-embedded. Honest, visible, fixable.
**Revisit at:** S5 (stratified sampling, canary/compare/rollback lifecycle).

## D-011 — Merge is merge-everything; GC retains N snapshots instead of taking leases
**S0.** Size-tiered merge selection with I/O and write-amplification budgets is S10; reader leases are S12. S0 needs *a* policy, so:
- **Merge:** collapse every part of a generation into one. Correct, trivially testable, and obviously the wrong policy at scale — which is fine, because S10 replaces the policy without touching the invariants.
- **GC:** reclaim only what no *retained* snapshot names, where retention is the last N snapshots (default 5). This is the S0 stand-in for invariant 6 ("old parts outlive max reader lease + grace"): with no lease service, snapshot retention is what guarantees a reader that pinned a snapshot still has its parts underneath it. Retention of 1 makes rollback impossible, which is why the default is not 1.

## D-012 — The duplicate policy is last-write-wins by `event_time`, ties to the later part
**S0.** The contract asks for duplicate behavior to be *documented*, not for duplicates to be impossible. This is the documentation. It is deterministic and it is tested.

## D-013 — Semantic grouping clusters the re-rank survivors
**S0.** The flagship aggregate — semantic `GROUP BY` over an arbitrarily large *filtered set*, with distributed-mergeable partial states — is S9. S0 delivers the *surface* over the survivors of a top-k: real clusters, real per-cluster scalar aggregates, real exemplar events. The API shape is the one S9 has to satisfy; the execution is not.
**This is stated as a limit in the README**, because a reader could otherwise reasonably believe the flagship feature is finished.

## D-017 — The default `nprobe` is derived from the tail, and carries a receipt
**S1. Architect's decision.** PRISM.md Part I §5.3: *"`nlist`/`nprobe` are outputs of recall, skew, filter selectivity and latency targets… No magic constants in docs or defaults without benchmark provenance."* S0's default of 4 was a round number that happened to be right. It is now a *derived* number that is right for a stated reason.

The rule: **the smallest `nprobe` whose p1 recall@10 clears 0.8 on the golden corpus.** Chosen on the **tail**, not the mean, because the mean is exactly what hid the failure in S0. `prism golden sweep` runs it and writes `testing/evidence/nprobe.json`; a test asserts `DEFAULT_NPROBE` still equals `chosen_nprobe` in that file, and that no smaller probe count clears the floor. The constant cannot drift away from its evidence without CI noticing.

The sweep (104 queries, 32 centroids) is unambiguous:

| nprobe | mean | p5 | p1 | min | queries returning **nothing** | scan fraction |
|---|---|---|---|---|---|---|
| 1 | 0.904 | 0.200 | 0.000 | 0.000 | **5** | 0.045 |
| 2 | 0.960 | 0.800 | 0.000 | 0.000 | **2** | 0.076 |
| 3 | 0.986 | 1.000 | 0.700 | 0.000 | **1** | 0.112 |
| **4** | **0.998** | **1.000** | **1.000** | **0.800** | **0** | **0.149** |

A mean of 0.904 at `nprobe=1` is how a system with five total failures in it gets described as "90% accurate".

## D-018 — The golden corpus asks hard questions on purpose
**S1. Architect's decision.** The S0 query set only asked *topic* queries — aimed at the middle of a behavioural motif. Those are easy, and they are not the queries that break.

The golden set now has three classes, and reports recall for each separately:

- **topic** (32 queries) — the middle of a cluster.
- **cluster-boundary** (56 queries) — half of one motif's vocabulary and half of another's, so the query vector lands *between* two centroids and its true neighbours are split across them.
- **hybrid** (16 queries) — meaning plus a scalar predicate.

At `nprobe=1` the classes diverge completely: **topic queries score a flawless mean of 1.000 with zero failures, while cluster-boundary queries fail outright on 5 of 56.** A benchmark that asked only topic questions would have measured recall 1.000 and concluded that a single probe was enough. That is the whole argument for the boundary class, and it is why `min`, `p1`, `p5`, `zero_recall_queries` and the five worst queries *by name* are now in every recall report.

Adaptive per-query probing — scaling the probe count when the centroid distance margins are tight, which is precisely the boundary case — is [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1), targeted at S6. It is deliberately **not** built in S1: it is a planner decision that must be costed against real kernel curves, and those do not exist until the CPU scan engine publishes them.

## D-020 — Format v3, and the mechanisms that should make it the last cheap bump
**S2. Architect's directive 2.** The compat corpus grows with every major version, and each one is a decoder we carry forever. v1 (JSON manifest) and v2 (binary manifest, block framing) are both still read. v3 must not start a habit.

So v3 ships **two mechanisms whose entire job is to make a v4 unnecessary**:

- **Feature-flag bits** for additions that change what stored bytes *mean*. An older reader refuses a bit it does not know rather than misreading the part — which is the only thing a version bump was ever really buying. `FEATURE_PROMOTED_COLUMNS`, `FEATURE_PARTITION_META`, `FEATURE_COLUMN_COMPRESSION` and `FEATURE_ENCRYPTION` are **reserved and deliberately unimplemented**: the bit numbers are nailed down now so two sprints cannot quietly claim the same one, and a part that sets one is refused by this build, which is exactly right — we cannot read it, and guessing would be worse than failing.
- **A TLV extension section**, with a `required` bit per extension id. A reader that meets an unknown *required* extension refuses the part; an unknown *optional* one it may skip. The mechanism ships with **zero** extensions defined, on purpose: the first user of an extension point must not also be the thing that breaks the format.

Plus four **reserved manifest words** that must be zero — a cheap slot for a future fixed-width field, guarded by a feature bit. A non-zero reserved word is *refused*, not ignored: ignoring is how a reader silently drops a field that changed what the data means.

S3's typed columns and S4's partitioning metadata are therefore **flagged extensions on v3**, not v4 and v5.

## D-021 — The block size is per-column, and 64 KiB was wrong
**S2.** Charter C-1 forced `BLOCK_SIZE` to be derived or dropped. Deriving it required making it *variable*, so it moved into the manifest per column.

The derivation (`testing/evidence/block-size.json`) is not close:

| block | manifest B/row | bytes read | read amp | p50 |
|---|---|---|---|---|
| 512 B | 16.06 | 51.1 MB | 32.6× | 5.8 ms |
| 1 KiB | 8.94 | 52.9 MB | 33.7× | 5.7 ms |
| 2 KiB | 5.38 | 58.1 MB | 37.0× | 6.1 ms |
| **4 KiB** | **3.62** | **69.2 MB** | **44.1×** | **6.8 ms** |
| 16 KiB | 2.30 | 136.8 MB | 87.2× | 11.7 ms |
| **64 KiB** *(the S1 pick)* | 2.05 | **387.6 MB** | **247.1×** | **29.7 ms** |
| 1 MiB | 1.98 | 2,695 MB | 1717.6× | 194.0 ms |

A 300-byte centroid range and a 256-byte rerank vector were each dragging a 64 KiB block off the disk behind them. End to end, `prism bench` p50 fell from **44.5 ms to 20.2 ms** and the scan rate doubled.

**The rule is constrained, and the constraint is the interesting part.** A naive "minimise bytes read" objective picks the smallest candidate every time — and it is *wrong*, because at a 2,000-row corpus the manifest term is invisible. The block directory is read **in full on every part open**, including opens that then prune the part away without touching a column; at 512-byte blocks that is ~16 bytes/row, so a billion-row part would carry a **16 GB directory** every reader must load before deciding the part is irrelevant. So: minimise bytes read, *subject to* the manifest staying under 4 bytes/row. The budget is a **policy** constant with a rationale; the block size is a **tuned** constant derived under it. That split is the whole point of C-1 — measurement answers the empirical question, and prose answers the one measurement cannot.

**The change also exposed a latent bug.** `read_range` computed a block's logical offset from the *global constant* rather than the column's actual block size. It was silently correct for exactly as long as every column happened to be 64 KiB, and the first store built at any other size returned whole blocks where a caller had asked for 256 bytes. Deriving a constant found a bug that assuming it had hidden.

## D-022 — The idempotency index is keyed by a composite string, not a tuple
**S2.** `BTreeMap<(String, String), Entry>` is the obvious model and it does not survive contact with JSON: object keys must be strings, and a tuple key serializes to `key must be a string`. The key is now `tenant \u{1f} idempotency_key` — the ASCII unit separator cannot occur in either field, so the composite is unambiguous. Caught the moment the index first hit disk, which is the only place it could have been caught.

## D-023 — The corpus generator draws S2 fields from a separate RNG stream
**S2. A near-miss worth recording.** Adding attributes and `observed_time` to `corpus::generate` consumed extra draws from the shared PRNG — which shifted the stream, changed every subsequent event's tenant, cost and body, and silently **changed the committed golden corpus**.

That is worse than it sounds. `make-fixtures` regenerates the corpus *and* its expected answers, so the drift check (`the_committed_golden_corpus_still_means_what_it_meant`) would have gone on passing **by construction**, while testing nothing at all. The S1 recall-tail finding — five cluster-boundary queries returning nothing at `nprobe=1` — would have quietly evaporated with it.

The S2 fields now draw from their own independent stream, and `testing/golden/v1/corpus.tsv` is **byte-identical to the one S1 committed**. A golden corpus that moves is not a golden corpus.

## D-024 — S2 builds the ingestion *semantics*, not the transport
**S2.** Named so nobody mistakes silence for completeness.

- **No network listener.** No OTLP/gRPC server, no HTTP endpoint. The OTLP **mapping** is real, tested against realistic OTLP/JSON GenAI payloads (including int64-as-string, which is what the protobuf JSON mapping actually emits and which a naive mapper silently drops), and events ingest from a file. The server is `prismd`, and it is S14.
- **No Kafka client.** The `Source` trait has exactly Kafka's offset semantics — poll, publish, *then* commit — and the file-backed source implements it, so invariant 7 is tested for real, through real process deaths. Wiring a broker is a transport detail. Getting the offset ordering wrong loses data permanently, and no amount of correct transport gets it back.

The S2 gate is about duplicate/replay semantics, offsets and fairness. All three are built and tested. The transport is a later sprint's problem, and saying so is cheaper than pretending otherwise.

## D-025 — Pagination semantics, pinned before anyone saw the syntax
**S3. Architect's directive 2.** Pagination is the part of a query API you cannot change later without breaking every client that ever used it. So it was pinned in [`docs/QUERY-CONTRACT.md`](QUERY-CONTRACT.md) **before** the SQL surface was exposed, and the code was written against the document.

- **One total order: `(score DESC, event_id ASC)`.** Ties break on `event_id`, always. For a scalar query every score is equal and the order collapses to `event_id ASC` — still total, which is all pagination needs.
- **A cursor is an opaque token binding a snapshot and a position**, plus a fingerprint of the plan. Paging continues to read *that snapshot*, not `CURRENT`.
- **An expired snapshot is an explicit error, never a different answer.** Silently continuing against `CURRENT` is how a client gets a page overlapping the last one, or skips rows that existed the whole time, and concludes the database is lying to them. It is.
- **No `OFFSET`**, and the error says why.
- **`S8 may EXTEND these semantics; it may not contradict them.`** That sentence is in the document, because by then there will be partners with cursors in their code.

**Pagination needed no new invariant.** Parts are immutable and a snapshot is a fixed set of them, so the answer to a query against a given snapshot is fixed forever — ingest publishes a *new* snapshot without touching the old one, merge writes *new* parts without touching the old ones, and GC only reclaims what no retained snapshot names. It needed the invariants we already had to be *true*.

**The bug this found:** the keyset skip was written as a tuple comparison, `(score, id) < (last_score, last_id)`. A tuple compares its first element **ascending**, and this order is `score DESC`. So a row with an equal score and a *smaller* id was treated as "after" the cursor — pagination rewound, repeated rows, and never terminated. It is written out longhand now, with the reason.

## D-026 — `DEFAULT_CANDIDATES` and `DEFAULT_RERANK`, swept jointly
**S3. Architect's directive 3.** Both were classified `policy` in S2, and the ledger admitted in writing that they were "empirical questions wearing a policy hat". They are now `tuned`, with a receipt (`testing/evidence/widths.json`).

**Swept jointly, and that is not a stylistic choice.** The controls interact: the candidate width decides *who is allowed to be reranked*, the rerank width decides *how many of them actually are*. A rerank budget of 200 buys nothing if only 50 candidates ever entered the heap. An independent single-axis sweep of either one measures a cross-section of a surface and reports it as the surface. `nprobe` is held at its own receipted value throughout.

Result: **candidates 200 → 50** (4× narrower, same recall, slightly faster); rerank stays at 50.

**And the honest part: the recall floors do not bind at all.** *Every* point in the grid clears them, because on this corpus PQ's top-10 already contains the true top-10. Left there, the rule would have chosen `rerank = 10` — the hard floor, since you cannot return ten hits from fewer than ten reranked rows — and that would be overfitting to a synthetic corpus with unusually well-separated motifs.

It would also have quietly broken pagination, which landed in the same sprint: **the paginated result set *is* the rerank survivor set**, so `rerank = 10` with a page size of 10 makes the first page the whole result and the cursor decorative. So the derivation carries a **policy** bound — `MIN_PAGEABLE_ROWS = 50`, five pages at the default page size — and *that* is what selects the value. Measurement could not see it. Prose can. Same shape as the manifest budget that constrains the block size (D-021).

## D-027 — The SQL surface is a compiler, not an executor
**S3. Architect's directive 4.** The gate risk was never the syntax; it was that a second door into the same engine might quietly become a second *engine*.

So the SQL layer **compiles to the `Query` the direct API already takes and calls the same executor.** The filter language lives in `prism-types::predicate`, *not* in the SQL crate — which makes "the same door" a fact about the type system rather than a slogan: the direct API builds exactly what SQL compiles to, because there is only one thing to build.

The parity tests assert **the physical-execution counters match, not just the rows.** If SQL ever grows its own scan, its own pruning, or its own idea of ordering, the counters diverge *before* the results do. Two doors into a database that disagree is a class of bug that takes years to find, because each door is individually self-consistent.

**Tenant policy is a shape, not a check.** The binder emits `(whatever the user wrote) AND tenant_id = <session tenant>`. The user's expression is a **subtree**, and a subtree cannot widen the conjunction it is nested inside — not with an `OR`, not with a `NOT`, not with an alias, not with parentheses. There is no list of escapes to enumerate and keep up to date; there is nothing to escape *to*. The same tenant value is also what drives partition pruning, so a query that somehow got past the row filter would still be reading only its own tenant's parts. Nineteen hand-written escape attempts and 8,000 fuzzed statements confirm it.

**The parser is network-facing input**, so S1's discipline applies in full: statement bytes, token count, expression depth, `IN`-list length and projection count are all bounded, and every bound is **named in its error** — "syntax error" is not something an operator can act on. The depth counter is the one that matters: a recursive-descent parser without one is a stack overflow waiting for `((((...))))`, and a stack overflow is a *process death*, not an error — it cannot be caught, reported, or attributed to the query that caused it.

**Two bugs found:** an ungrouped aggregate over an empty set returned **zero rows** instead of one row saying zero (which makes "nothing matched" indistinguishable from "the query failed"), and `ORDER BY` was silently ignored rather than refused (which would let a caller believe they had asked for an order they did not get).

## D-028 — Partition metadata lives in the **catalog**, not in the part manifests
**S4. Directive 2 forced this, and it is the most consequential design decision in the sprint.**

The directive: *"'physically impossible' must be testable, so define it as an I/O property: a query's execution trace never touches a byte range belonging to another tenant's partition. The strongest gate test: fill other tenants' partitions with unreadable garbage — every tenant-A query still returns correct results, because it never looked."*

Until S4, pruning opened **every part's manifest** to decide which parts to skip. That means a tenant-A query already read bytes belonging to tenant B's parts — so the isolation claim was false in exactly the sense the directive cares about, and one corrupt part anywhere broke every query in the store.

**Pruning that must open a manifest to decide whether to open a manifest is not isolation.**

So the partition key, tenant list and zone map now live in the **catalog snapshot** (`PartEntry::Located`), above the parts. A part outside a query's partitions is never opened, never checksummed, never read. `a_query_never_touches_another_tenants_partition_even_if_it_is_garbage` shreds every non-alpha partition — manifests, columns, everything — and alpha's scalar, semantic, hybrid and aggregate queries all still answer correctly.

Pre-S4 snapshots deserialize as `PartEntry::Legacy` (a bare part id) and fall back to opening the manifest, which is exactly the old behaviour. Old stores keep working; they simply do not get the new guarantee until a merge rewrites them.

**A second property fell out for free:** the blast radius is now localized **per column** as well as per tenant. Corrupting `pq.codes` in one of bravo's parts breaks bravo's *similarity search* and leaves bravo's `COUNT(*)` working — because a count does not read the compressed codes. "Tenant bravo cannot run similarity search on this partition" is a far more actionable thing to tell an operator than "the store is corrupt".

## D-029 — Attribute promotion is a schema *event*, and the two representations are a dual door
**S4. Directive 4, and [issue #2](https://github.com/Bobcatsfan33/PrismDB/issues/2).**

Promotion is **versioned and generation-like, never an in-place rewrite.** A part is written either promoting a key or not; existing parts are never touched. So the two representations — a typed top-level column and an entry in the attribute map — **coexist across parts of different ages**, and every reader dispatches on which one a given part uses. A merge is what migrates a part forward onto the current promotion scheme, exactly as it migrates a format version or a generation.

**The promoted key is removed from the map** in a part that promotes it. Storing it twice would make promotion cost storage rather than save it, and leave two sources of truth for one value. That is precisely why the S4 extension is **required**: a reader that ignored it would decode the map, not find the key, and report it as *absent* — silently wrong answers rather than an error.

**The dual-door test is the S4 gate that matters most.** Two stores, identical but for promotion; every query must return identical rows **and identical logical counters** — same parts opened, same rows scanned, same rows passing the filter. If promotion changed what the engine *considered*, it would be a different query wearing the same text.

`physical_bytes_read` is the **one counter that legitimately differs**, and the test asserts it differs *downward* — because if promotion did not read fewer bytes, it bought nothing. Measured: **181,489 → 118,440 bytes (−35%)** on a two-key predicate, with an identical answer.

That assertion caught a real bug immediately. The first implementation re-read all three column files **on every row**, so promotion read *more* bytes than the map it replaced. Asserting the win, rather than assuming it, is what found that. It also exposed that `COUNT(*)` was materializing every column of every row to answer a question about the *number* of rows — now it materializes nothing.

**Promotion is invisible to an event.** An event read out of a promoted part is byte-identical to the same event read out of a mapped one; the promoted key is re-attached into the map on read. Promotion is a storage decision, not a schema change, and if it were observable every equivalence in the system would quietly stop holding.

## D-030 — The shared-bucket seam: query-layer isolation is complete; disk-layer co-tenancy is documented and accepted
**S4. Directive 3 required a deliberate choice, and this is it.**

In a **shared** bucket, part-level metadata describes the *bucket*, not the tenant: one `time_min`/`time_max` pair, one cost range, one union attribute-key dictionary, one set of centroid ranges, one tenant list. **Every one of those tells tenant A something about tenant B.**

Three options were on the table: scope the metadata per tenant; document and accept the leak; or abolish shared buckets (which destroys the small-tenant economics that shared buckets exist for). We did the first for everything a query can observe, and the second — explicitly — for what remains.

### What is scoped (enforced)

Every part carries a **per-tenant section** (`TenantStats`) in the S4 extension: rows, time range, cost range, error/success flags, and **the attribute keys that tenant uses**. A query reads *its own section and no other*.

- *"Does this part contain key X?"* is answerable **per tenant**. Tenant A cannot learn that tenant B uses `b.secret.key`.
- A **zone map is a zone map for one tenant**. If A's rows span one minute and B's span an hour, A skips the part on a query outside A's minute — even though the part as a whole overlaps. This both closes the leak *and* prunes better, which is the pleasant case where the secure thing is also the fast thing.
- No count, row, error message or counter reveals another tenant's data. `a_shared_bucket_leaks_no_metadata_through_any_query_surface` asserts it.

### What is **not** hidden (documented and accepted)

The **union attribute-key dictionary** and the **tenant list** are in the manifest bytes. They have to be: the dictionary is what *decodes* the attribute column, and the tenant list is what *prunes* the part. So:

> **An operator with raw disk access to a shared bucket can see which tenants share it, and the union of their attribute keys. No query can.**

That is the seam, stated plainly rather than pretended away. The threat model is explicit:

| layer | co-tenancy visible? |
|---|---|
| any query surface (rows, counts, errors, counters, `EXPLAIN`) | **no** — enforced, tested |
| raw disk / backup access to a shared bucket's manifests | **yes** — accepted |

**Two mitigations, both real.** A tenant who cannot accept this gets a **dedicated bucket** — `--dedicated whale` — and shares a part with nobody; `a_dedicated_bucket_shares_a_part_with_nobody` enforces that a dedicated bucket holding two tenants is *refused at commit*, because if it were accepted every isolation claim resting on dedicated buckets would be false and nothing would notice. And S14's envelope encryption closes the disk layer properly, per tenant.

**Bucket assignment is SHA-256, not a fast hash.** A tenant must not be able to *choose* which bucket they land in by choosing their id: co-tenancy is our decision, not theirs, and an attacker who can steer themselves into a chosen victim's bucket has turned a metadata question into a targeting one.

## D-031 — Duplicate reconciliation is partition-scoped
**S4. A consequence of partitioning that had to be found, and was — by a failing test.**

A merge collapses each *partition*; it never collapses partitions into each other, because a part spanning two buckets is a part a query for one tenant has a reason to open on behalf of another. So merge-time duplicate reconciliation ([D-012](DECISIONS.md): last-write-wins by `event_time`) now only sees collisions **within a partition**.

A duplicate whose `event_time` moved it into a different time window would therefore survive as two rows. **This is prevented at admission, not at merge:** the S2 idempotency index refuses same-key-different-content as an `idempotency_conflict` and dead-letters it. A *replay* — same key, same content — necessarily has the same `event_time`, therefore the same window, therefore the same partition, and *is* reconciled. Merge-time reconciliation remains exactly what it always was: the backstop for duplicates that predate the idempotency window, which by construction share a partition.

The only way to produce a cross-partition duplicate is the S0 loader (`prism ingest`), which bypasses admission **on purpose** — it is a bulk loader, not a production path. Noted here so nobody rediscovers it as a bug.

## D-032 — An unknown CLI flag is an error
**S4.** Found while demonstrating promotion: `--promot gen_ai.system` was **silently ignored**, and the store was created without the promotion its operator asked for. They would have discovered it months later, from a query reading more bytes than it should.

A silently-ignored flag on a command that *defines a store's configuration* is not a usability wart, it is a correctness bug with a very long fuse. Unknown flags are now rejected, with a suggestion.

## D-033 — The candidate heap breaks ties on `event_id`, not on physical position
**S4. A real bug, found by the recall floor, in code that had a comment claiming it was fine.**

The bounded candidate heap ordered candidates by `(dist, part, row)`, with this comment:

> *"Ties break on (part, row) so the candidate set is deterministic."*

**Deterministic, yes — in the layout.** Which is precisely the error [D-008](DECISIONS.md) corrected in the final sort, surviving in the one place D-008 never reached. The query contract says the total order is `(score DESC, event_id ASC)` and that *"order must be a function of the data, never of the layout."* The heap is **bounded**, so it does not merely *order* the answer — it decides **which tied rows are allowed to be answers at all**. A layout-dependent selection means two stores holding identical rows answer the same query differently, and a merge changes an unchanged answer.

This is not an exotic case. Real telemetry repeats bodies verbatim — the same retry, the same timeout, the same tool error, a thousand times a day — so identical vectors, identical PQ codes and **exactly equal** distances are ordinary, and a top-k is routinely a *choice* among hundreds of tied rows.

**How it surfaced.** S4 repartitioned by time window, which changed nothing about the data and everything about the layout. `p1 recall@10` on the bench corpus fell from **1.00 to 0.60**, and five queries returned rows that were not in the exact oracle's answer at all. The diagnostic that named it: on the same corpus, with the same codebook, the pre-S4 and post-S4 engines scanned **the same 3,880 rows**, considered **the same 50 candidates**, and returned **the same top score (0.9258)** — with **different event ids**. And raising `nprobe` from 4 to 8 doubled the scan fraction and changed recall *not at all*, because the rows were never being **missed**. They were being **outvoted by their addresses**.

**The fix:** distance ties break on `event_id`, so the candidate set is a function of the data. The scan therefore needs event ids, not just the rerank — the id column joins `event_time` and `tenant_id` as a scalar the scan reads. The distance is compared *before* an id is materialized, so a candidate that loses on distance alone (nearly all of them) never allocates.

**It costs 19%** — bench `query_p50` 21.9 ms → 26.1 ms on the same machine, same corpus. That is the honest price of an answer that does not depend on where its rows are stored, and it is worth it: the alternative is a database whose answers change when it tidies up. Making the scan cheaper again is the SIMD sprint's problem (S6), which is where scan kernels belong.

**Why the existing tests all passed.** Every suite ran green — including the golden-corpus recall gate, because the golden store is a *single part* and a single part has no layout to disagree about. The bench, which builds a multi-part store, was the only thing that could see it, and the only reason it *did* see it is that the recall report carries `p1` and `min` and not just the mean. **The mean was 0.965.** A mean-only report would have shipped this.

Two tests now bind it, and both were verified to **fail against the old code** before being committed:
- `the_same_rows_laid_out_differently_give_byte_identical_answers` — one corpus, four layouts (one window vs days vs hours, one ingest vs many), 1 part to 55, byte-identical answers required. It varies the *batching* as well as the window, because a layout test that varies only the window scans tied rows in time order every time and proves nothing — the first version of this test did exactly that, passed against the bug, and had to be sharpened.
- `a_merge_moves_rows_between_parts_and_changes_no_answer` — the same property along the axis a running store actually travels.

Both receipts (`nprobe`, `widths`) were **re-derived against the fixed engine**, since a selection rule changing is at least as material as the corpus changing. Both chose the same constants: `nprobe = 4`, `candidates = 50`, `rerank = 50`.

## D-034 — The training sample is keyed on `event_id`, not on position
**S5. The C-4 audit's first find, and the worst one available.**

The codebook training sample was a reservoir keyed on **index into a vector built by reading parts in catalog order**. So the same rows, laid out differently, trained a **different codebook** — different coarse centroids, different PQ sub-quantizers, and therefore *a different meaning for every byte in the store*.

This is D-033's defect in the one place it would have been hardest to ever notice. A wrong answer from the candidate heap is at least a wrong answer to a question somebody asked. A codebook that depends on the layout is a store whose bytes quietly mean something else after a merge, and nothing anywhere reports it.

**The fix:** **bottom-k by `sha256(seed ‖ event_id)`** — a reservoir that does not care what order it sees the rows in, because the chosen set is a pure function of the *set* of rows. Stratified across partitions, with an equal floor before proportional allocation, so a store whose loudest tenant emits 100× the rows of everyone else does not get a codebook that describes only that tenant.

Hashed cryptographically, for the same reason bucket assignment is: **an id must not be able to steer itself into or out of the codebook's training set.**

`the_same_rows_train_the_same_codebook_under_every_layout` asserts it directly on the generation id — which *is* the content hash of the codebooks, so if the ids match, every centroid is byte-identical.

## D-035 — Merge duplicate reconciliation breaks `event_time` ties on the content hash
**S5. The C-4 audit's second find.**

The duplicate policy was *"last write wins by `event_time`; **ties go to the later part**"* — a tie broken on physical position, in a comment that stated it as a feature. Two stores holding identical rows would reconcile the same duplicate pair differently depending on which part each copy happened to land in, so a merge's output depended on the layout of its input rather than on its content.

Ties now break on the **content hash**, which is a total order on the data. Two events with the same id, the same `event_time` *and* the same content hash are byte-identical, so which one wins is not observable — which is what a tie-break should mean.

## D-036 — k-means restarts, because a codebook must not depend on a lucky draw
**S5. Found by fixing D-034, which is the interesting part.**

The moment the training sample became order-independent, **recall fell below its floor** — `p1 recall@10` 0.70 against a floor of 0.80. Nothing had broken. The old input order had simply been handing k-means++ a *lucky first point*, and the corpus-ordered sample flattered it.

That is worth stating plainly: **the recall we had been reporting was, in part, a coincidence of the input order.** A pipeline whose quality depends on a lucky draw is not a pipeline.

k-means now runs `KMEANS_RESTARTS = 5` independent seeded inits and keeps the codebook with the lowest **inertia**. Deterministic, offline, linear in the constant — and the quality now comes from the data rather than from the draw.

**And then `DEFAULT_NPROBE` had to be re-derived**, because the codebook changed and a codebook is the PQ geometry. Which is exactly the standing obligation the architect had just added as directive 3, arriving in the same sprint that created it. See C-3.

## D-037 — Generation lifecycle state lives in the catalog snapshot
**S5.**

The generation *record* is content-addressed and immutable: it **is** its codebooks. A lifecycle *state* — candidate, canary, active, deprecated — is not a property of the codebooks; it is a fact about the store at an instant. So it lives in the snapshot.

Which buys the whole lifecycle for free: **every transition is one atomic catalog commit**, and **rollback restores the states along with the parts**, because they are the same object. There is no half-migrated flag, no repair path, and no state in a writer's memory. A crash in the middle of a migration leaves orphan parts and the old snapshot live — the same thing every other writer here does, because it is the only thing that is safe.

`with_training` provenance is deliberately **not** in the content address. Two byte-identical codebooks are the same generation no matter what story is told about how they were trained; folding provenance into the id would mean identical codebooks hashed to different ids, and parts pinned to them would stop being interchangeable for no reason a reader could see.

## D-038 — Drift baselines are generation-scoped, and a baseline that cannot be rebuilt goes DEGRADED
**S5. Directive 2, decided as instructed.**

A **baseline is a statement about a distribution in one embedding space.** When the space changes underneath it, the baseline is not *stale* — it is **meaningless**, and invariant 9 forbids comparing across it. A novelty score is a score.

So:

- `NOVELTY` / `SEMANTIC_DIFF` evaluate a generation's events **only** against **that generation's** baseline. Never cross-generation. Not "close enough". During the mixed window each generation runs against its own baseline, and the two are never merged into one number.
- **A re-embed migration is not complete until every baseline has been recomputed under the new generation.** The naive definition — *"no part references the old generation"* — is **necessary and not sufficient**, and shipping only that would have broken drift detection *silently*: the alarm would have gone on producing numbers, every number would have been nonsense, and nobody would have been told. `migration_status` refuses to say `complete` while a baseline still points at a dead space.
- **When a baseline cannot be rebuilt, the alarm goes `DEGRADED` and says so on every evaluation.** This happens for a real and unavoidable reason: rebuilding a baseline means re-embedding the historical rows, re-embedding needs the **raw bodies**, and raw bodies expire under retention because prompts contain secrets. The rows survive; the text they were embedded from does not; so those rows can never be re-embedded into any new space, by anyone, ever.

  `DEGRADED` names the baseline, the generation, the reason, and **how many rows are going unwatched**. It does not return zero. It does not return the old numbers. It does not return nothing. `prism drift check` **exits non-zero**, because an alarm that is not running is an incident, not a quiet day.

**A drift alarm that quietly stops firing is worse than one that was never configured, because a configured alarm is trusted.** That sentence is the entire justification for the state existing.

## D-039 — A bridge fuses ranks, never scores
**S5. Directive 4's second half.**

A cross-space query is **refused** by default — that is the correct behaviour and the common case. A cosine of 0.83 in one model's space and 0.83 in another's are two different numbers that happen to print the same, and averaging them is not a merge, it is a category error with a plausible-looking result.

A **bridge** is a catalog-registered declaration that two spaces may be answered together: explicit (somebody declared it), validated (it carries a note), and **named in the output** so a bridged answer can never be mistaken for a native one.

**The only implemented policy is `rank_fusion`, and it does not merge scores at all.** Each space answers the query natively — its own embedding, its own codebooks, its own cosines, entirely inside its own geometry — and the two *rankings* are fused (reciprocal-rank fusion). A rank is unitless. This **obeys** invariant 9 rather than working around it, and a policy that averaged scores across spaces would be forbidden by the generation contract even if somebody implemented it.

The score in a bridged result is the **fusion score, not a cosine**, and `bridge` is set on the result so nobody can mistake it for one. Silence about that would be a lie by omission.

## D-040 — Redaction is a rewrite, and it is one-way
**S5.** Retention expires raw bodies (PRISM.md: *"Raw-body retention is policy-controlled (prompts contain secrets)"*).

Immutability is law, so redaction edits nothing: it **rewrites** the affected parts without their bodies and swaps the catalog, exactly like a merge. The old parts sit there untouched until GC is separately asked, which is the only reason it is safe to run at all.

**The vector stays. The text does not.** That asymmetry is the whole story: the store can still answer questions about what those events *meant*, and can **never again ask a different model what they meant**. Redaction is therefore the one operation in this system that permanently forecloses a future migration for the rows it touches — which is why it demands a recorded reason, and why the parts it produces carry a **required** format extension (`EXT_S5_LINEAGE`).

Required, and not ceremonially: a reader that skipped it would see a column of empty strings and could not distinguish *"this event had no body"* from *"this event's body expired"*. A migration that could not tell those apart would either dead-letter a partition it should have reported as un-re-embeddable, or — if some future embedder tolerated empty input — write a part full of meaningless vectors and call the migration a success. The format refuses to let a reader be unaware.

## D-041 — A bootstrap codebook depends on arrival order, and the lifecycle is the answer
**S5. The C-4 layout gate found this, and it is the most interesting thing it found.**

The gate materializes one frozen logical corpus under four physical layouts and demands identical answers. Three of them failed, and the cause was not a tie-break: **the bootstrap generation is trained on the first batch**, so ingesting the same corpus in one batch or in twenty produces different codebooks, different PQ codes, and different approximate answers.

**No amount of C-4 discipline can fix that, because you cannot train on data that has not arrived.** The first codebook is necessarily a function of what the store had seen when it trained one.

So the gate states what is actually true, in two halves, and both are strict:

1. **The exact path is layout-invariant always** — even on a provisional store. It uses no codebook, so there is nothing to hide behind: if two stores holding identical rows disagree here, the disagreement is in the *selection*, the *ordering* or the *reconciliation*, and every one of those is a D-033-class bug.
2. **The approximate path is layout-invariant once the store is settled** — that is, once a generation has been trained from a stratified sample of the *whole store* and migrated onto. That is what the lifecycle is *for*, and it is the state any store that has run a migration is in.

Anything weaker would be a gate that passes by being vague. Anything stronger would be a promise the arrow of time does not allow.

**And it exposed a second, fixable violation on the way.** The training sample was stratified **by partition** — `tenant-bucket × time window` — and a time window is a *store configuration*. Two stores holding identical rows with different window sizes therefore got different strata, different samples, and different codebooks. That is charter C-4 one level up: **the strata themselves must be a logical property of the data**, or keying the sample on `event_id` buys nothing. Strata are now **tenants**. A tenant is a fact about a row; a time window is a fact about a config file.

## D-042 — The restart sweep chooses a plateau, not the best point
**S5. A rule that had to be rewritten after it picked a lottery ticket.**

The obvious rule for [`KMEANS_RESTARTS`](../testing/evidence/kmeans-restarts.json) is *"the restart count with the best derived probe count"*. It is a trap, and the sweep is jagged enough to spring it: on the golden corpus 1 and 2 restarts need **7** probes, **3 needs only 3**, and 5 through 25 all settle on **6**.

Winner-takes-all would have picked 3 — a single lucky draw that no larger restart count reproduces.

> **We are choosing a method, not a lottery ticket.**

Selecting the luckiest point on the grid would have reintroduced *exactly* the dependence on a fortunate init that this constant exists to remove, and the next corpus would not be lucky in the same place. The rule is now: **the smallest restart count that begins a plateau** — one whose derived probe count is matched by every larger point on the grid. A plateau is the signature of a method that has stopped depending on its draw.

This is charter C-3's "a boundary optimum is presumptively an artifact", generalized: **a *singleton* optimum is an artifact too.**

## D-043 — `DEFAULT_NPROBE` rose from 4 to 6, and that is the honest number
**S5.**

Fixing C-4 in the training sample (D-034) and removing the lucky-init dependence (D-036) changed the codebook, and the codebook is the PQ geometry. `DEFAULT_NPROBE` was re-derived against it, exactly as charter C-3 (extended in S5) requires, and it moved from **4 to 6**. Mean scan fraction went from 0.146 to **0.203** — about 39% more data touched per query.

**The engine did not get worse. The measurement got honest.** The old value was derived against a codebook that a layout accident had flattered: the training vectors arrived in corpus order, which handed k-means++ a lucky first point and produced centroids that happened to align with the corpus's motifs. Nothing about that was a property of the data, and nothing about it would have survived a merge, a repartition, or a differently-batched ingest.

What we bought for the 39%:

- The codebook is now a **function of the data**, so the same rows train the same codebook under every layout — asserted directly on the generation id, which *is* the content hash of the codebooks.
- The **tail improved**: `p1 recall@10` at the default is now **1.00**, where the old configuration cleared its floor at 0.80.
- Stratification by tenant means the loudest tenant no longer writes the codebook on everyone else's behalf.

A number that goes up when you stop fooling yourself is the correct number.
