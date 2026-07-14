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

The rule: **the smallest `nprobe` whose p1 recall@10 clears 0.8 on the golden corpus.** Chosen on the **tail**, not the mean, because the mean is exactly what hid the failure in S0. `prism golden sweep` runs it and writes `testing/golden/nprobe-provenance.json`; a test asserts `DEFAULT_NPROBE` still equals `chosen_nprobe` in that file, and that no smaller probe count clears the floor. The constant cannot drift away from its evidence without CI noticing.

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

The S2 fields now draw from their own independent stream, and `testing/golden/corpus.tsv` is **byte-identical to the one S1 committed**. A golden corpus that moves is not a golden corpus.

## D-024 — S2 builds the ingestion *semantics*, not the transport
**S2.** Named so nobody mistakes silence for completeness.

- **No network listener.** No OTLP/gRPC server, no HTTP endpoint. The OTLP **mapping** is real, tested against realistic OTLP/JSON GenAI payloads (including int64-as-string, which is what the protobuf JSON mapping actually emits and which a naive mapper silently drops), and events ingest from a file. The server is `prismd`, and it is S14.
- **No Kafka client.** The `Source` trait has exactly Kafka's offset semantics — poll, publish, *then* commit — and the file-backed source implements it, so invariant 7 is tested for real, through real process deaths. Wiring a broker is a transport detail. Getting the offset ordering wrong loses data permanently, and no amount of correct transport gets it back.

The S2 gate is about duplicate/replay semantics, offsets and fairness. All three are built and tested. The transport is a later sprint's problem, and saying so is cheaper than pretending otherwise.
