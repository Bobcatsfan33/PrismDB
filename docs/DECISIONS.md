# Decisions

Judgment calls made where [PRISM.md](PRISM.md) is silent. The rule: **where the contract has an answer, follow it; where it does not, choose the boring option and write it down here.** A decision recorded badly is worse than no decision, so each entry says what was chosen, what was rejected, and what would make us revisit it.

---

## D-001 — `docs/PRISM.md` is the pasted v2 text, not a file copied off disk
**S0.** The build instruction said to copy `06-prism-overview-and-roadmap-v2.md` from the working directory. That file does not exist anywhere on the machine; the v2 master document was supplied inline instead. `docs/PRISM.md` is therefore that text, verbatim and unedited.
**Revisit if:** the canonical file turns up and differs. Diff it against `docs/PRISM.md` before trusting either.

## D-002 — Two dependencies: `serde` and `serde_json`. Nothing else.
**S0.** The charter says dependency-light. Manifests, catalog snapshots, generation records and CLI output are all JSON, and hand-rolling a JSON parser would be strictly worse than using the standard one — more code, more bugs, no benefit.

Everything else is implemented in-tree, on purpose:
- **CRC-32 and SHA-256** (`prism-types::hash`) — invariant 8 (content-addressed codebooks) and invariant 10 (checksums cover stored bytes end to end) are load-bearing. They are verified against published test vectors, so this is not "trust me", it is "here is the NIST vector".
- **The PRNG** (`prism-types::rng`) — every stochastic step must be reproducible from a seed or the baselines and the recall contract are not reproducible either.
- **Argument parsing** — a flag parser would have been the largest thing in the dependency tree, for ten subcommands.

**Rejected:** `clap`, `rand`, `sha2`, `crc32fast`, `thiserror`, `anyhow`.
**Revisit if:** a hand-rolled primitive shows up as a hotspot in a profile (then swap the *implementation*, keeping the tests), or the CLI grows a surface that genuinely needs a parser.

## D-003 — The rerank tier is full float32, in a separate file
**S0.** PRISM.md Part I §5.2 lists four options for the exact-rerank tier (full vectors cold, fp16 with an accuracy contract, re-embed on demand, residual quantization) and deliberately does not choose. S0 has to store *something*, and the choice freezes into the part format.

Chosen: **full float32, in its own column file (`vectors.f32`), never on the scan path.** It is the boring option and the only one with no accuracy contract to negotiate: the re-rank is exact because the vector is exact. Keeping it in a separate file rather than interleaved is what makes the tier separation physical — the scan reads `pq.codes` and never opens `vectors.f32`, and `bench` reports the two byte counts separately, always.

**This is the biggest open cost question in the project.** Full float32 is ~3.07 PB per trillion vectors against ~96 TB for the codes — a 32× multiple that dominates the storage bill, and "cost per billion events retained and queryable" is one of the three numbers the company watches. S0 does not decide it; S0 makes it *measurable* (`rerank_tier_multiple` in `baselines.json`) and *swappable* (one file, one reader).
**Revisit at:** S1, before the part format hardens, and again at S11 when the two-tier cost becomes a product surface.

## D-004 — Whole-file CRC now; block framing at S1
**S0.** Each column file carries one CRC-32 over its whole contents in the manifest. The contract wants checksummed *block* framing so that damage localizes to a block (Part III §9) — that is explicitly an S1 deliverable, and building it now would mean designing the block header twice.
**Consequence today:** a single flipped byte invalidates a whole column rather than a 64 KiB block. Acceptable at S0 scale; not acceptable at object-storage scale.
**Revisit at:** S1.

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
