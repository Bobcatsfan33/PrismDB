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

## C-6 — C-1 receipts re-derive at the end of every sprint that materially changes the engine
**S7. Architect's standing rule, arising from [D-048](DECISIONS.md).**

A receipt is evidence only if it describes the engine that actually ships. S6 proved this the hard way: re-running the block-size sweep against the SIMD engine *panicked*, because the manifest had grown since the receipt was written — the evidence had silently stopped matching the code ([D-048](DECISIONS.md)).

> **At the end of every sprint that materially changes the engine, the format, or a corpus, every C-1 receipt is re-derived.** It is a line in the sprint checklist, not a thing anyone remembers to do.

"Materially changes" is any of: a new or altered kernel or reduction; a format or manifest change; a new codebook generation; a new or reversioned corpus; a new execution substrate. The re-derivation either **reconfirms** (the constants did not move — which, when the change is bit-identical like SIMD, is the *prediction*, checked) or **re-derives** (they moved, and the new value ships with the engine that produced it). Either way the committed evidence and the shipping engine agree, which is the whole point of C-1.

This subsumes the earlier per-amendment obligations — C-3's "re-sweep on a real-embedding corpus" and the generation-conditional re-sweep — into one ritual: **if the engine changed, the receipts are suspect until re-run.**

## C-7 — A randomized algorithm's answer is a function of its data, not its seed or its scan order
**S9. Architect's standing rule, the randomized-algorithm edition of [C-5](DECISIONS.md).**

S9 introduces the engine's first *randomized* algorithm — k-means over PQ codes. Randomization is where determinism contracts usually die: change the seed, the init draw, the iteration order, or a parallel reduction, and the clusters move. C-5 said the answer is a function of the data and not of which ISA computed it. C-7 extends the same law across the two new axes a randomized aggregate opens:

> **A randomized aggregate is a deterministic function of `(logical row set, parameters, generation)`. Its seed is derived from that content — never from a clock, a counter, or a machine id — and every order it depends on (initialization draw, mini-batch consumption, partial-state merge) is a *logical* order (`event_id`, or partition id), never a physical one (scan order, arrival order).**

The two failure modes it forbids, concretely:

- **A wall-clock or counter seed** would make the clusters a function of *when* or *where* you asked — the same class of defect as a layout-dependent tie-break, one axis over. The seed is `SHA-256(sorted event_ids ‖ k ‖ generation)`; identical inputs seed identically, forever, everywhere.
- **Consuming rows in scan order** would make the clusters a function of physical layout — which part, which block, which route fetched the row. Mini-batches are drawn in `event_id` order, and distributed partials merge in partition-id order, so the physical plan that produced the rows is invisible to the clustering, exactly as it is invisible to a top-k (C-4) and to a SIMD sum (C-5).

The gate is the strong form of §2, now over clusters: **identical clusters, exemplars, and per-cluster aggregates** across every layout-variant fixture and across forced plan-flips and route-flips. See [determinism contract §13–§15](DETERMINISM-CONTRACT.md) for the mechanism and [D-058](DECISIONS.md)/[D-059](DECISIONS.md) for what building it taught.

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

## C-5 — The answer is a function of the data, not of which instruction set computed it
**S6. Architect's standing rule, and the float edition of [D-033](DECISIONS.md)/[C-4](DECISIONS.md).**

The [determinism contract](DETERMINISM-CONTRACT.md) is written before the first SIMD kernel and carries charter weight:

> Identical event IDs, identical ordering — **the same answer, byte for byte** — across scalar, AVX2, AVX-512 and NEON, on the layout-variant golden fixtures.

Not "scores within tolerance". A database that answers a question differently depending on which CPU ran the query is not one database, it is a family of subtly disagreeing ones — and the disagreement surfaces exactly where a bounded top-k turns a one-`ulp` score difference into a different *selection*.

**The strong form is achievable, not aspired to**, because of how the two float reductions are defined:

- **The ADC scan** is defined as `Σ table[j·256 + code[j]]`, summed strictly ascending, no FMA, no reordering. The reduction is *per row*, so the SIMD parallelism lives **across rows, one row per lane** — each lane runs the identical scalar chain, and lane-wise IEEE addition is correctly rounded and identical to scalar. Bit-identical on every ISA, no epsilon ([D-044](DECISIONS.md)).
- **The rerank dot product** is defined as the scalar sequential sum, and computed *only* by the scalar reference on every ISA — because its natural SIMD form is a horizontal tree reduction, a different order, and it runs over tens of rows, not millions. So it is ISA-invariant by being the same code.

Consequently **a whole query is bit-identical across ISAs**, which is what makes the C-1 receipts survive the sprint (C-5's own §5): the answers did not move, only their speed.

The **weak-form fallback** — scores may differ but *selection* may not, proved on a boundary-tie stress corpus — exists for a future 4-bit-shuffle kernel whose `uint8` arithmetic cannot be IEEE-bit-identical. S6 ships entirely strong-form; the weak form is the gate the first such kernel must pass, accepted only with a logged decision.

Two mechanical rules ride along: **an ISA CI cannot execute does not ship enabled** (AVX-512 is behind `experimental-avx512` until a runner can run it), and **the quoted p50 is the worst supported ISA's, never the best's**.

## D-044 — SIMD kernels are bit-identical by vectorizing across rows, not within a row
**S6.**

The obvious way to SIMD a PQ scan — the 4-bit-shuffle trick — quantizes the lookup table to `uint8` and uses saturating byte adds. It is fast and it is **not IEEE arithmetic**, so it cannot be bit-identical to a float reference. That is a real kernel, and it is not the one S6 ships.

S6 ships the **honest** kernel: vertical vectorization. Process L rows per iteration, one per lane; for each sub-quantizer `j`, gather the L table values and add them into L accumulators. Each lane accumulates its row's `m` values in the identical ascending order the scalar reference uses, and the only floating-point operation is a lane-wise add — which IEEE-754 requires to be correctly rounded and identical to the scalar add. **The gather changes which value is added, never how.** So every lane is bit-identical to scalar, on AVX2 (hardware gather), NEON (manual gather — no NEON gather instruction), and AVX-512.

`kernel::tests::every_available_kernel_is_bit_identical_to_the_reference` compares **bits**, not values, across awkward tails and several `m`. The whole-query gate (`determinism.rs`) compares answers over the frozen corpus on every ISA the machine supports; the x86 and ARM CI runners between them cover every shipping kernel on real silicon.

**Honest speed finding:** on this engine's shape (`m = 8`, `dim = 64`), the NEON kernel with manual gather is **not faster** than the compiler's autovectorization of the scalar loop — the baseline reports NEON at 28.2 ms vs scalar 27.8, and the headline quotes the worse (NEON) per C-5. The sprint's deliverable is *bit-identity and the contract that enforces it*, not a speedup; a genuine win wants a wider `m` or a real hardware gather, and it will still have to pass this gate.

## D-045 — The candidate top-k holds indices, not owned event ids
**S6. What the allocation-free requirement forced, and it was an improvement.**

The determinism contract §4 requires the block scan and the top-k to allocate **zero** times across a full golden run. The scan already did (it writes distances into a reused buffer). The top-k did not: its `Candidate` owned an `event_id: String`, allocated on every insert.

So `Candidate` became `Copy` — `{ dist, part, row }`, owning nothing — and the tie-break borrows the event id out of the already-resident scalar column via a closure the bounded heap holds. A row entering the top-k now allocates nothing. This required a purpose-built `TopK` (an explicit binary max-heap whose comparator reaches *outside* the element) rather than `BinaryHeap<T: Ord>`, whose comparator cannot see external state — and it is faster besides, having dropped a string clone per candidate.

A counting allocator (`crates/prism-engine/tests/alloc.rs`, its own `#[global_allocator]` so the shipped binary is untouched) offers the real `TopK` 50,000 rows and asserts zero allocations, and offers the real kernel a full range and asserts zero.

## D-046 — Adaptive probing is monotone-only in v1, and its margin is corpus-conditional
**S6, [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1).**

A query near a cluster boundary has its true neighbours split across centroids, so the base `nprobe` reaches some and misses the rest. Adaptive probing widens the probe count for exactly those queries: a centroid beyond the base is also probed when its distance is within `(1 + ADAPTIVE_MARGIN)` of the base's boundary.

**v1 is monotone-only: it may add probes above the base, never subtract.** So recall can only improve, and **every existing `nprobe`/width receipt stays valid as a floor** — which is why the receipts are measured with adaptive *off*. The cost-reduction direction (fewer probes on easy queries) is deferred to the real-embedding corpus, because it tunes against cluster geometry the hash embedder cannot represent.

The margin is a **tuned** constant with a C-3 wrinkle: on the hash corpus the recall floor at the shipping base is *already met flat*, so measurement cannot pick the margin by benefit — every margin gives identical shipping recall. It can only see cost. So the value (0.05) is selected by a **policy cost bound** (adaptive must not exceed 1.5× the flat probe count at the shipping base), with the mechanism *validated* by showing it recovers a deliberately-starved base's tail — proof the heuristic fires on real boundary queries. The sweep also *proves monotonicity* by refusing any margin that lowers shipping recall. Corpus- and generation-conditional; the benefit-driven derivation is [issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3).

## D-047 — mmap makes truncation a named error by bounds-checking, not by a signal handler
**S6.**

The framed column read path now maps files read-only instead of `pread`-ing them. Parts are immutable, so a read-only mapping is the easy case — with one sharp edge: **a truncated file under mmap `SIGBUS`es on access**, and a `SIGBUS` is a process death that cannot be caught, reported, or attributed.

The [S1 truncation discipline](DECISIONS.md) — a truncated part names its column, block and byte range — survives **by construction, not by a signal handler**: the map's length *is* the file's real length, every access goes through a bounds check against it, and a range past the end is the same named `Corrupt` error it always was. The `SIGBUS` is unreachable because the check fires first. `fuzz::a_truncated_framed_column_under_mmap_names_itself_and_never_sigbuses` truncates a real framed column at many lengths and asserts every read either succeeds or names itself — and that the process survives, which is the whole point.

The mmap FFI is declared in-crate (`mmap`/`munmap`, the stable POSIX ABI) rather than pulling `libc`, keeping the dependency tree at serde ([D-002](DECISIONS.md)). Every `unsafe` block is in [UNSAFE-INVENTORY.md](UNSAFE-INVENTORY.md), enforced by a CI grep gate.

## D-048 — `BLOCK_SIZE` re-derived 4 KiB → 2 KiB, and the budget was budgeting the wrong bytes
**S6. Directive 5 (re-run C-1 sweeps against the SIMD engine) surfaced a latent bug.**

Re-running the block-size sweep against the current engine **panicked**: no candidate fit the 4-byte/row manifest budget. The cause was not SIMD — it was that S4 and S5 had added per-tenant stats and lineage extensions to the manifest, all **fixed overhead independent of block size**, and the sweep was budgeting the *whole manifest*. At a 2,000-row corpus that fixed overhead now swamps the budget.

But the budget was only ever about the **block directory** — the term that is read on every open *and scales with block size*. The extensions do not scale with block size and do not belong in it. So the budget now isolates the directory term by a delta (manifest size at this block size minus the floor at the largest block size, which cancels the fixed overhead), and under the corrected budget the sweep re-derives cleanly to **2 KiB** — interior to the grid, reading fewer bytes than 4 KiB.

This is exactly what C-5 §5 predicted a re-run would do: *reconfirm or re-derive, either way the evidence matches the shipping engine.* Every recall-derived constant (`nprobe`, widths, restarts, adaptive) **reconfirmed exactly**, because the kernels are bit-identical; only the byte-cost-derived `BLOCK_SIZE` moved, and it moved because the *manifest* cost model changed, which measurement caught the moment it was asked to.

## D-049 — fp16 rerank is the first negotiated accuracy contract
**S7. Directive 4, and the precedent for every lossy encoding after it.**

Storing rerank vectors in fp16 halves the exact-tier storage bill — the biggest open cost question in the system — at the price of approximation. That is a [D-003](DECISIONS.md) event, a change in *what a stored byte means*, and it is handled the way D-003 built the format to handle it: the rerank-tier descriptor declares `encoding_id` and `accuracy_contract_id`, and the reader **dispatches on the pair**, refusing any pairing it does not implement.

**fp32-exact remains the only default.** fp16 is opt-in behind `encoding_id = 2 / accuracy_contract_id = 2`, and a build that does not implement the contract refuses the part rather than guessing at what its scores mean. Three things were updated in the *same change*, deliberately, to set the precedent: the **contract text** (the descriptor + the tolerance constant), the **evidence** (`testing/evidence/fp16.json`), and the **unknown-encoding fixture** (both the unit test and the fuzz sweep now treat encoding 2 as *known* and moved the "unknown" probe to encoding 3). Add a lossy encoding without updating its fixture and the fixture silently stops testing anything.

**The honest finding: fp16 is not strict-order-stable, and it cannot be.** Any lossy encoding reorders rows whose exact scores fall within its rounding error — that is unavoidable. So "selection stability" is defined as the *achievable* guarantee: **fp16 never inverts a pair whose fp32 scores differ by more than the tolerance.** Rows within the tolerance of each other are, by the contract, interchangeable — opting into fp16 *is* agreeing that such rows may reorder.

That guarantee is not asserted, it is *derived*: if the tolerance exceeds **twice** the worst per-score gap, then two rows separated by more than the tolerance in fp32 have fp16 scores that still differ, same sign, so fp16 cannot invert them. Measured worst gap on the golden corpus is `4.6e-4`, so the floor is `9.2e-4`; the committed tolerance is `2e-3`, with headroom. The receipt proves selection stability holds at that tolerance, and the tolerance is a C-1 tuned constant bound to it.

## D-050 — the rerank route is invisible to the answer; selection-identity, not score-identity
**S7. Directives 2–3, and the reason pagination survives a route change.**

A GPU reduces sums in a different order than a CPU — tree, not sequential — so its scores differ in the last bits. A GPU **cannot** be bit-identical to a CPU, and pretending otherwise is how S7 would have shipped a lie. So the device edition of the determinism contract weakens §2's strong form to exactly what is achievable and sufficient:

> **Scores may differ within a documented tolerance; the returned event ids and their order may not.**

This is **selection-identity**, the device analogue of the weak form, and it rests on [charter C-4](DECISIONS.md) already being law: distance ties break on `event_id`, never on a score's last bit, so a sub-tolerance score difference cannot reorder the answer. The gate proves it on the golden, layout-variant, and boundary-tie corpora — the CPU route and the CPU-reference-of-the-GPU route return **byte-identical event-id lists**, scores within tolerance.

**The `GpuReference` route is not a pretend GPU.** It is the *definition* the real CUDA kernel will have to prove itself equal to — the scalar-kernel-for-SIMD pattern, one substrate up — and it models the one thing that matters for correctness: a different, deterministic reduction order (pairwise), so the selection-identity gate exercises a *real* score difference rather than a no-op. The suite refuses to pass if the two routes ever produce bit-identical scores, because then the tolerance path was never tested.

## D-051 — a device fault degrades to CPU; per-tenant admission lands with the kernels
**S7. Directive 6.**

A CUDA error — OOM, launch failure, device lost mid-query — **degrades to the CPU path with a logged event.** Never a failed query, never a wrong answer, never corrupted engine state. The GPU is an accelerator, not a dependency: a query answerable on the CPU is always answered. This is why the rerank fetches every candidate's vector *first* and reranks second — a device fault is then a pure recompute on the already-fetched vectors, no re-read. Fault injection walks all four phase boundaries (upload, kernel, selection, download) and asserts each returns the *CPU answer*, degraded and logged.

**Device-memory admission is per tenant, and lands with the kernels.** A device OOM caused by tenant A must not fail tenant B — the same starvation isolation the ingest path enforces, now on the device. A query's device footprint is admitted against a per-tenant share *before* upload, and a tenant over its share is degraded to CPU rather than permitted to eat into another tenant's guaranteed portion. The reservation releases on drop, so a query cannot leak device memory even if it errors mid-flight.

## D-052 — the cursor pins the route
**S7. What the route-flip pagination gate forced, and it is the right design.**

Directive 3's gate — *paginate with the route forced to flip between pages* — failed the obvious implementation, and the failure is instructive. The cursor stores a **score** for keyset pagination, and scores differ by route; page 1 on the GPU ends at a GPU score, page 2 on the CPU compares CPU scores against it, and the boundary row is included or excluded wrongly — a duplicate or a gap.

The fix is the one the cursor already uses for the snapshot: **pin the route.** A paginated query is *one logical query*, so its route — like its snapshot — is fixed at the first page and carried in the cursor. On resume the pinned route wins over any planner or test override, so the boundary scores on both sides of a page break come from the *same* route and cannot disagree. Flipping the external route between pages is then invisible: the cursor holds the route steady, and the pages tile the answer exactly — no duplicate, no gap. "A cursor must survive a route change" is satisfied by the cursor *being* what survives it.

## D-053 — S7 ships "GPU-ready, GPU-off", and does not claim the gate
**S7. Directive 2's explicit fallback, taken honestly.**

CI GPU capacity is a sprint deliverable, and this environment cannot deliver it: no CUDA hardware, no cloud credentials to provision a runner. The architect anticipated exactly this — *"if the runner cannot be secured this sprint, S7 pivots to 'GPU-ready, GPU-off' and does not claim the gate."*

So it does not. What ships is **everything device-agnostic**, fully built and tested against the CPU reference: the route abstraction, selection-identity, fault containment, per-tenant admission, the fp16 accuracy contract, and the cost-model *mechanism*. What does **not** ship is the CUDA kernel (declared behind the `cuda` feature, not compiled — writing untestable FFI would be the faked completeness the project refuses) and the GPU runner (provisioning-as-code in `infra/gpu-runner/`, reviewable and one `terraform apply` from real, but **not applied**). The crossover thresholds are placeholders marked device-conditional and un-derived, because deriving them requires measuring a GPU. `Route::Cuda` and the GPU are **off by default** — the AVX-512 rule, device edition: an instruction set, or a device, that CI cannot execute does not ship enabled.

The 4-bit-shuffle CPU kernel — the genuine SIMD speedup S6's NEON finding pointed at — is filed as an issue with that finding attached, not built. One new execution substrate per sprint (directive 7).

## D-054 — Plan-invariance: three strategies, one candidate set, by construction
**S8. Directive 1, the sprint's central gate — [D-033](DECISIONS.md) in its plan edition.**

A semantic query with a predicate runs three ways — **interleaved** (filter fused into the distance scan), **scalar-first** (filter, then distance the survivors), **semantic-first** (distance, then predicate only the rows near enough to enter the heap). They are three physical strategies for **one logical query**, and the plan may cost differently but **may not answer differently** ([query contract §9](QUERY-CONTRACT.md)).

It holds *by construction*, not by luck: all three offer the **identical passing rows, ranked by the identical PQ distance, to the identical bounded top-k**. They differ only in *when* the predicate runs relative to the distance — which changes the work, never the set. The gate forces every strategy on the golden, layout-variant, and boundary-tie corpora and asserts byte-identical event ids and order; a second test proves the *work* genuinely diverges (scalar-first computes far fewer distances, semantic-first far fewer predicate evals) so the strategies are not a distinction without a difference.

**Consequence: the cursor need not pin the plan.** Unlike the route — whose *scores* differ, so its cursor pins it ([D-052](DECISIONS.md)) — the plan changes no score, so a page-2 keyset boundary is identical whichever strategy computed it. The gate paginates while flipping the plan between pages and the pages tile the answer exactly. We proved it before relying on it, per the directive.

## D-055 — The optimizer consumes receipts; its regret is worst-cell, not average
**S8. Directives 2 and 3.**

The plan choice is a cost decision over one estimate — **selectivity**, the fraction of probed rows the predicate admits. A selective predicate favours scalar-first (distance only survivors); a permissive one favours semantic-first (the distance narrows, the predicate is barely consulted). The estimate is deliberately **crude** (directive 7: choose among three strategies well, do not research cardinality estimation) — a bounded sample of real rows, not a histogram — and it carries a receipt saying so.

The metric is **worst-cell regret, not average** (directive 3): across the selectivity matrix, the chosen plan must be within `PLAN_REGRET_BOUND_PCT = 15%` of the best fixed plan's *actual* cost in **every** cell. An optimizer that wins on average by losing badly in one cell is worse than a fixed heuristic for the customer in that cell. Cost is a **deterministic proxy from the actual counters** (`distances_computed · w_d + predicate_evals · w_p`), so the gate has no wall-clock noise — it measures whether the crude estimate is good enough to pick within the bound.

**The regret gate earned its keep immediately.** It caught a real cost-model bug: semantic-first's predicate saving materializes only when the heap *fills*, which takes ~`cap/selectivity` probed rows — modelling `cap` instead of `cap/sel` mis-picked semantic-first at low selectivity, for 76% regret. The worst-cell metric surfaced it where an average would have buried it.

**The cost-model coefficients are policy, informed by a microbench, not bound to a stopwatch.** The honest finding ([`cost-model.json`](../testing/evidence/cost-model.json)): a real interpreted `predicate::eval` costs *as much as or more than* a SIMD-batched ADC distance, which is why "distance-first, predicate-lazily" tends to win. A constant bound to an exact timing would be a flaky gate, so `DIST_COST_MILLI`/`PRED_COST_MILLI` are committed as stable in-magnitude values with the microbench as documentation; engine-conditional per C-6.

**The GPU axis stays inert.** `GPU_MIN_CANDIDATES` is `usize::MAX` and `gpu_available()` is false, so the planner never steers on a coefficient that has no evidence — the GPU route is cost-model-ineligible until the runner produces real crossover receipts, at which point the optimizer ingests them as data with zero code change (directive 2).

## D-056 — Query semantics stated in the contract before the code
**S8. Directive 4.** [query contract §10–§14](QUERY-CONTRACT.md) extends the S3 contract (never contradicts it) with:

- **Null semantics** — an unwritten attribute is *absent*, not a three-valued `NULL`; a comparison against absence is false. Deliberately two-valued, because SQL `NULL`-propagation in a filter that decides tenant visibility is a footgun. The surprising corner (`!=` on an absent attribute) is stated loudly rather than discovered.
- **Tie semantics** — C-4's `event_id` rule, now a SQL guarantee: `ORDER BY score DESC, event_id ASC`, always, a query may not override it.
- **Threshold × top-k** — the result is the top-k of the rows clearing the threshold: threshold first, `LIMIT` second, applied to the *exact* score, never the PQ distance.
- **Generation selection** — same-space parts merge, cross-space is refused, and the error is written to **teach**: it names both spaces, explains that a cosine of 0.8 in one is not a cosine of 0.8 in the other, and offers the three fixes (name a space, declare a bridge, finish the migration). Invariant 9 surfacing where a SQL user first meets it.

## D-057 — EXPLAIN carries estimates and actuals; the Flight door is the same door
**S8. Directives 5 and 6.**

**EXPLAIN** reports the optimizer's estimate alongside the query's actual for the four controls and the physical work, plus the chosen route and plan **with the reason** (§14). A calibration harness tracks the estimate-vs-actual selectivity error across the matrix in CI, so cost-model drift is a visible number, not a slow surprise — and it caught its own subtlety: `actual_selectivity` from counters is strategy-dependent (semantic-first observes a biased near-subset), so the harness measures the *true* rate from a forced-interleaved run.

**The Flight SQL door is the same door** ([D-057 detail in `flight.rs`], directive 6): the tenant conjunction is injected *below* it, its counters are byte-identical to the direct API and the SQL text door on every query, and its decode obeys S1's bounded-allocation discipline (every length capped, every violation named; garbage never panics). **What ships is the door's server-side query path, not the Arrow IPC / gRPC transport** — real Arrow Flight needs the `arrow`/`tonic`/`prost` ecosystem the serde-only charter ([D-002](DECISIONS.md)) excludes, and a network server the roadmap defers to S14. That transport, and the dependency decision it forces, belong to the sprint that needs it; building an untested wire format now would be the faked completeness the project refuses. The *same-door* property — the thing that matters for correctness — is built and proven three-way today.

## D-058 — `semantic_cluster` seeds from content and consumes rows in logical order
**S9.** The determinism mechanism behind [C-7](DECISIONS.md). A randomized aggregate has two extra ways to leak the physical world into the answer — the seed and the consumption order — and both are closed the same way the rest of the engine closes them: with a *logical* fact about the data.

The PRNG is seeded from `SHA-256(sorted event_ids ‖ k ‖ generation)`, never a clock or a counter, so identical inputs seed identically forever; and the fit streams rows in `event_id` order, so the float reductions in the centroid update are reproducible and layout-independent. The fit is a **streaming Lloyd's** — each pass reassigns every row and recomputes each centroid as the logical-order mean, holding only `k·dim` accumulators plus a batch — which is honest about what it is: not sub-epoch mini-batch SGD, but a bounded-state full-pass k-means that scales in memory the way mini-batch promises to. It reuses [`kmeans_minibatch`] with k-means++ init; **restarts** make the fit a function of the data rather than of a lucky draw ([D-036](DECISIONS.md)'s lesson, now on the aggregate).

## D-059 — Exemplars are exact and C-4; the ARI oracle is ground truth, not another approximation
**S9.** Two judgment calls about *legibility*, which is the product.

An exemplar is chosen on the **exact** rerank distance to its centroid, ties broken on `event_id` ([C-4](DECISIONS.md)) — never on the PQ distance the membership is decided by. A cluster is *labeled* by its exemplar, and a mislabeled cluster is a wrong answer a human reads and believes, worse than a slow one; the PQ code is cheap enough to cluster a billion rows by, the exact vector is accurate enough to name one with.

And the clustering oracle is **ground-truth labels**, not a reference clusterer. PRISM.md names sklearn; the charter forbids the dependency, and it turns out not to want it: synthetic labeled clusters *are* the exact answer sklearn would only estimate, so `ARI(ours, truth)` is a real number. The frozen corpus ([`testing/cluster/v1`](../testing/cluster/v1), C-2) carries the adversarial shapes a round-blob demo would hide — Zipf-skewed sizes, touching boundaries, and uniform noise where the honest answer is **low confidence**, asserted (`quality = 1 − inertia_k/inertia_1` below the floor) rather than dressed up as `k` confident groups.

## D-060 — The aggregate is bounded before it exists — `k` and a state budget
**S9.** The S2 lesson (bound the aggregate before it can OOM the node), applied to clustering state. `k` is capped by policy (`MAX_SEMANTIC_K`, a number a human can read), and the clustering working set is admission-controlled against `SEMANTIC_STATE_BUDGET_BYTES`: a `semantic_cluster` whose `(rows, dim, k)` would exceed the budget is **refused with a named limit**, never silently clamped and never left to OOM. A clamp answers a different question than the one asked; a refusal is honest about the limit. The budget is what makes "cluster an arbitrarily large filtered set" a bounded promise instead of a denial-of-service.

## D-061 — Partial states merge in canonical order, and it is a property, not a hope
**S9.** The aggregate is built to distribute ([PRISM.md](PRISM.md) S12) before it distributes: each logical shard produces a partial state (per-cluster count, scalar sums in logical order, a bounded exemplar selection), and the global answer is their **merge in canonical (shard-id) order**. Because a float sum is not associative, the merge is defined as a fixed fold over shard id ascending — the physical order partials arrive in cannot change the result — and the property test asserts exactly that: the same partials in a scrambled order produce byte-identical aggregates. Distributed *model fit* is not in S9; the model is fit single-node and only the aggregate is partitioned-and-merged, which is the part that has to be equal to a single-node answer for the distribution to be honest.

## D-062 — S9 does not claim the 100M-row < 10s gate; the scale profile is filed
**S9. The honest wall, in the shape S7 taught.** The clustering quality that clears the `ARI ≥ 0.8` floor on the corpus needs five restarts of a fifteen-epoch fit — ~75 full passes — and at the measured **~10⁴ rows/s single-core** ([`semantic-cluster.json`](../testing/evidence/semantic-cluster.json)) that projects to ~10⁴ s for 100M rows, a ~1000× gap. So the gate is **not claimed**, exactly as S7 did not claim the GPU gate it had no hardware to measure. What *is* built is the mechanism the target needs — the bounded-state streaming fit, the mergeable partials, the deterministic answer — and what is filed is the scale profile it does not tune this sprint: a single-restart / fewer-epoch fit for scale, PQ-code ADC assignment (no exact-vector reconstruction), SIMD, multi-core, and the streaming PQ-code fit that also removes the 512 MiB working-set bound. A benchmark that reports 10⁴ rows/s and a 1000× gap is worth more than a claim of 10s that no committed number supports.

## D-063 — The novelty primitives reuse the drift baseline; the SQL keyword surface is deferred
**S9.** `NOVELTY … AGAINST` is the S5 drift [`Baseline`](../crates/prism-part/src/baseline.rs) asked a per-row question, and `SEMANTIC_DIFF` is the S9 aggregate asked a comparative one — neither is new machinery, which is the directive's point (two primitives on structures that already exist). Both obey invariant 9 absolutely: a `NOVELTY` scoring rows against a baseline in another embedding space is refused with the teaching error a cross-space ranking gets, because a cross-space distance is exactly as meaningless as a cross-space score. The injected-novelty benchmark holds **precision and recall ≥ 0.9 on the worst seeded class** (the S1 tail lesson) — with a corpus-conditional caveat recorded in the receipt: a few synthetic novel classes collide into the baseline's hash buckets and are not actually far, so the benchmark injects the classes that genuinely are, because labelling a colliding class "novel" would benchmark the toy embedder's collisions, not the alarm.

**And the SQL *keyword* surface is deferred, like S8's Flight transport.** The S9 semantics — determinism, ordering, ephemeral ids, bounding, invariant-9, exemplars — are built and gated at the engine level (`Engine::semantic_cluster` / `novelty_against` / `semantic_diff`), where correctness lives, and every gate test cites its query-contract clause. The grammar that types them (`GROUP BY semantic_cluster(…)`, `NOVELTY … AGAINST`, `SEMANTIC_DIFF`) is the next increment: the semantics ship first and are proven, the surface that spells them follows. The binder says so where it refuses, rather than pretending the door is open.

## D-064 — A deleted row leaves the drift baselines at the next scheduled snapshot, not at merge time
**S10.** The directive demanded this be *decided*, not left silent, because [S14](PRISM.md) deletion compliance inherits it. Two readings are defensible — a deleted row could leave the NOVELTY / drift baselines when the reconciling **merge** physically removes it, or at the next scheduled **baseline snapshot** regardless of merge timing. We choose the **baseline snapshot**.

A baseline is already a scheduled, generation-scoped, frozen artifact ([D-038](DECISIONS.md)); its recomputation reads the live parts and now **skips tombstoned rows**, so a deletion takes effect for drift at the next `baseline refresh`, whenever that runs. Tying baseline membership to merge timing instead would couple two schedulers that must not see each other — the merge scheduler and the baseline scheduler ([merge contract §2](MERGE-CONTRACT.md)'s coupling rule) — and would make a compliance-relevant fact ("is this deleted row still influencing our drift detection?") depend on physical merge cadence, which is exactly the sort of hidden dependence the charter exists to forbid. So a deletion is effective **for query answers at tombstone commit** (the search path filters tombstoned ids at once), **for the drift baselines at the next baseline recompute**, and **for bytes-on-disk at merge**. Three clocks, each named, none secretly driving another.

And a delete never touches a frozen artifact: tombstones live on the live catalog snapshot, so the [C-1](DECISIONS.md)/[C-2](DECISIONS.md) receipt corpora and golden answers — committed immutable bytes — are untouched, and the drift check that compares committed bytes still compares the same bytes.

## D-065 — The S3 client is hand-rolled, and TLS is the one honest exception
**S11. The dependency decision the directive demanded, made against the [D-002](DECISIONS.md) charter.** This is the first substantial external dependency on the **read path of the truth**, so *why it is trustworthy enough for that* has to be reviewable.

The charter is serde-only, and everything else — CRC-32, SHA-256, the PRNG, the SQL parser, the `statvfs`/`statfs` shim ([S10](MERGE-CONTRACT.md)) — is hand-rolled in-tree and verified against published vectors. The S3 client follows that rule: a **minimal, hand-rolled S3 client** over the subset the engine needs (`GET` with `Range`, `PUT`, conditional `PUT`, `HEAD`, `DELETE`, list, multipart), speaking **HTTP/1.1 over a raw `TcpStream`**, signing with **AWS SigV4** composed from the existing in-tree `sha256` (HMAC-SHA256 is a thirty-line composition over the SHA-256 already verified against FIPS vectors), and parsing the handful of XML fields the responses carry with a small in-tree scanner. Rejected: `aws-sdk-s3` / `object_store` / `reqwest` — each drags in tokio, a TLS stack, hyper, and a hundred transitive crates, and the charter's whole thesis is that the read path of the truth should have an auditable, minimal trusted base. A from-scratch S3 client is more of *our* code, but it is code we can read end to end, and for the path that decides what the database says, that is the trade the charter already made five times.

**The one exception, named honestly: TLS.** A TLS 1.3 stack is not something to hand-roll — getting it subtly wrong is a security hole, not a bug — so **real S3-over-WAN (HTTPS) is the one place a TLS dependency (or the platform's TLS) is unavoidable**, and it is deferred with the MinIO increment (§1 of the [storage contract](STORAGE-CONTRACT.md)). CI's MinIO speaks **plain HTTP on localhost**, which the hand-rolled client handles with no TLS at all, so the *client, the SigV4 signing, and the S3 semantics* are exercised end-to-end against a real S3-API server without a TLS dependency; the HTTPS transport for a production remote is the small, well-scoped place the exception lands, decided when that remote is exercised. Building an unaudited TLS stack now to avoid ever taking the dependency would be exactly the faked self-sufficiency the project refuses.

## D-066 — Catalog publication is a compare-and-swap, via conditional put
**S11.** Until S11 the `CURRENT` pointer was swapped by an atomic local rename ([catalog](../crates/prism-part/src/catalog.rs)) — atomic, but **last-writer-wins**: two writers both renaming `CURRENT` both "succeed", and one commit is silently lost. On object storage there is no rename, so the mechanism is replaced by a **conditional put** (CAS):

- **Create** (first publication of a key) uses **`If-None-Match: *`** — the put succeeds only if the object does not exist. Two racing creators: one wins, the other gets `412 Precondition Failed` and retries against the now-existing state.
- **Replace** (advancing `CURRENT` from snapshot *A* to *B*) uses **`If-Match: <etag-of-A>`** — the put succeeds only if `CURRENT` still holds exactly the version the writer read. A writer whose read is stale fails the precondition and re-reads, so a lost update is a *detected conflict*, never a silent overwrite.

**MinIO / S3 semantics, stated because they are load-bearing:** S3 has supported `If-None-Match: *` on `PUT` (conditional create) since 2024, and `If-Match` on `PUT` (conditional overwrite); MinIO implements both. Where a backend lacks conditional overwrite, the fallback is a **version-guarded** put (read the version id, write a new object, CAS the pointer) — the same CAS discipline, one indirection more. The local `ObjectStore` backend implements CAS with an **`O_EXCL` create** (create-if-not-exists is the filesystem's `If-None-Match: *`), which is what proves the *semantics* in this sprint's gates; the S3 wire form is verified against MinIO in the filed increment. The primitive is `ObjectStore::put_if_absent` (and its guarded-replace sibling), and it is the only way `CURRENT` moves, so a race for publication is always resolved, never lost.
