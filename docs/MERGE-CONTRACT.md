# The Merge & Lifecycle Contract

**Status:** written in S10, before the scheduler — the discipline every other contract ([ingestion](INGESTION-CONTRACT.md), [query](QUERY-CONTRACT.md), [generation](GENERATION-CONTRACT.md)) was written under. Merges, GC, reader leases, and deletes are the machinery that keeps a store *stable under sustained mutation*, and stability is a property you promise before you build, or you discover you did not have it in production. This contract binds that machinery; the scheduler is written against it.

The one fact the whole document rests on, unchanged from S0: **nothing is mutated in place, and nothing is deleted except by GC.** A merge, a re-embed, a delete, a rollback — every one of them writes new immutable parts and performs **one atomic catalog commit** ([invariant 3](PRISM.md)). So every clause below is ultimately about two things: *which* new parts get written and *when* the old ones are allowed to disappear.

---

## 1. A merge is new parts plus one atomic commit — and it is crash-atomic by construction

A merge reads some parts, writes fewer/larger ones, and commits a snapshot that names the new parts instead of the old. The old parts are **not retired at commit** — they remain on disk, still named by retained older snapshots, until GC (§5) separately reclaims what no retained snapshot names ([invariant 5](PRISM.md): GC is never in the publish path).

Crash-atomicity is not added by the scheduler; it already holds and the scheduler may not weaken it:

- A part is written into a `<id>.tmp/` directory, fsynced, then **renamed** into place ([`part.rs` `PartWriter::write`](../crates/prism-part/src/part.rs)). A crash before the rename leaves a `.tmp` orphan no snapshot names — invisible, GC-reclaimable, never opened.
- A snapshot file is written-and-renamed **before** the `CURRENT` pointer is swapped ([`catalog.rs` `commit_meta`](../crates/prism-part/src/catalog.rs), [`io::write_atomic`](../crates/prism-part/src/io.rs)). A crash between them leaves `CURRENT` naming the old snapshot — a clean old-or-new, never a hybrid.
- The commit **pre-opens every part it is about to name** and refuses to commit if any fails to open. A half-written part can never enter a snapshot.

The gate for this is the fault matrix ([`faults.rs`](../crates/prism-cli/tests/faults.rs)) and the 10,000-run kill/reopen campaign, extended in §3 to a fault it never covered.

## 2. Merge selection is size-tiered with budgets — and it is *explainable*, not deterministic

Merges are **size-tiered per partition**: parts within a partition are bucketed into size tiers, and a tier is merged when it accumulates more than a tier-ratio's worth of parts, producing one part that graduates to the next tier. The scheduler spends three budgets and refuses to exceed any: an **I/O budget** (bytes moved per cycle), a **write-amplification** cap (bytes written ÷ bytes read), and a **concurrency** limit (merges in flight).

The requirement on a merge decision is **explainability, not determinism**. Query answers are already layout-invariant ([C-4](DECISIONS.md)/[C-5](DECISIONS.md)/[C-7](DECISIONS.md)) — a merge cannot change an answer, only the physical layout an answer is computed over — so there is nothing for merge-order determinism to protect. What matters instead is that a human can reconstruct *why* the scheduler did what it did:

> Every merge decision records its inputs — the partition, the tiers and their part counts, the merge debt, the impurity, and every budget and how much of it the decision spent — in enough detail to **reproduce the decision** from the record alone.

This is deliberately *weaker* than deterministic scheduling, and deliberately so. Coupling the scheduler to a deterministic global order would make it depend on things it must not see — wall-clock arrival, which node holds a partition, the order two tenants' merges happened to be considered — the same class of coupling [C-7](DECISIONS.md) forbids in the aggregate. The scheduler sees tiers, debt, and budgets; it logs them; it does not promise to pick the identical partition twice given a different arrival order.

## 3. ENOSPC is a first-class fault: merge admission, and named backpressure

**This clause exists because the storage engine's own build host ran out of disk during this project, and a database must beat the standard its build environment failed.** A merge is the disk-hungriest thing the engine does — it writes a second copy of the data it is compacting before it can free the first — so a merge is exactly where "the disk filled" must be a *decision*, never an accident.

- **Merge admission.** Before a merge starts, the scheduler estimates the merged part's output size and requires **projected output + a safety margin** to fit in currently-free disk space. If it does not, the merge is **refused with a named condition** (`merge deferred: insufficient free space`) and **not started** — never started-and-stranded with a half-written part and a full disk.
- **Backpressure is a taxonomy of named conditions, not a crash.** Under disk pressure the engine degrades in named, documented ways — **ingest refusal** (`disk_pressure`, the ingest analogue of `quota_exceeded`) and **merge deferral** — and it **never corrupts**. When space returns, it **recovers unaided**: the deferred merge is simply admitted on a later cycle, because merge carries no state between attempts (§1 — it is a pure function of the parts it reads).
- **A real disk-full is injectable and tested.** The abort-based kill points ([`faults.rs`](../crates/prism-part/src/faults.rs)) model a *crash*; ENOSPC is a *returned error* and needs its own injection. A fault hook makes `io::write_atomic` and the part writer return an out-of-space error on demand, and the matrix fills the disk **mid-merge, mid-part-write, and mid-catalog-commit** and asserts: the operation fails with a named error, the store still opens and `verify()`s, it lands on old-or-new-never-hybrid, and it succeeds unchanged once space returns. The temp+rename discipline (§1) makes this safe by construction; the tests make it *proven*.

## 4. The scheduler is tuned constants, and every one of them has a receipt

Tier ratios, the merge-debt threshold, the I/O and concurrency and write-amplification budgets, the reader-lease duration, the GC grace — the scheduler is constants all the way down, and every one obeys [C-1](DECISIONS.md): a committed receipt with the evidence or the rationale, [C-3](DECISIONS.md) policy bounds each with a written reason measurement cannot see, and the boundary-artifact check — **a threshold that lands on the edge of its sweep is a missing constraint, not an optimum**, and the sweep is widened or the constraint is found before the receipt is committed.

## 5. Reader leases and GC grace — invariant 6, by construction

[Invariant 6](PRISM.md) says old parts outlive the maximum reader lease plus a grace period. Until S10 there was no lease service; `gc --retain N` snapshots was the S0 stand-in. S10 makes the lease real, and makes invariant 6 hold **by construction rather than by coincidence**:

> **GC grace is *derived* from the maximum lease duration, not tuned independently.** There is **one** constant — the lease duration — and the grace is a function of it. Two independently-tuned numbers can drift apart until grace < lease and a reader's parts vanish underneath it; one constant and a derivation cannot.

- A reader **pins a snapshot** for its query lifetime ([invariant 4](PRISM.md), [query contract §2](QUERY-CONTRACT.md)); a paginating reader's cursor carries the pinned snapshot id. GC may reclaim a snapshot only once it is older than the lease-plus-grace horizon, so a live reader within its lease always finds its parts.
- **A crashed reader leaks its lease, and the lease expires anyway — bounded.** Because a lease is time-bounded, not held open by a live connection, a reader that dies mid-pagination does not pin the snapshot forever: its lease expires within the bounded duration, GC then proceeds, and the dead reader's stale cursor returns the **explicit expired-snapshot error** ([query contract §2](QUERY-CONTRACT.md)) — *"the snapshot this cursor names has been reclaimed; re-run the query"* — never a wrong answer, never a silent gap. Invariant 6 holds throughout: the parts survived exactly as long as the lease promised and no longer.

## 6. Deletes are tombstones, reconciled at merge — and when a deleted row leaves a baseline is *decided*, not silent

A user delete does not rewrite or erase a part in place. It writes a **tombstone** — a durable record that a set of `event_id`s is deleted as of a snapshot — and the tombstone is **reconciled at merge**: the merge that rewrites a tombstoned row's part simply does not carry that row forward. Reconciliation is **idempotent** (replaying a tombstone drops the same rows and no others) and last-writer-wins by identity, never by physical position ([C-4](DECISIONS.md)). Between the tombstone commit and the reconciling merge the row is **logically deleted** — excluded from query answers — while still physically present; merge reclaims the space later, on its own schedule.

**The decision the directive demands, written down** ([D-064](DECISIONS.md)): a deleted row leaves the NOVELTY / drift baselines at the **next scheduled baseline snapshot**, not at merge time. A baseline is already a scheduled, generation-scoped, frozen artifact ([D-038](DECISIONS.md)); its recomputation honours tombstones — a logically-deleted row is not counted — so a deletion takes effect for drift at the next baseline recompute, **regardless of when the reconciling merge runs**. Tying baseline membership to merge timing would couple two schedulers that must not see each other (the merge scheduler and the baseline scheduler — the §2 coupling rule again), and would make a compliance-relevant fact ("is this deleted row still influencing our drift detection?") depend on physical merge cadence. S14 deletion compliance inherits this: deletion is effective for baselines at the baseline schedule, for query answers at tombstone commit, and for bytes-on-disk at merge.

**A delete may never silently mutate a frozen artifact.** The [C-1](DECISIONS.md)/[C-2](DECISIONS.md) receipt corpora and golden answers are immutable committed bytes; a delete operates on a live store, never on `testing/golden/**` or `testing/cluster/**`, and the drift check that compares committed bytes ([C-2](DECISIONS.md)) still compares the same bytes.

## 7. Fairness is a bounded delay, and write amplification is an observable

A saturating large tenant **cannot starve** a small tenant's merges or queries. This is stated as a **bounded delay**, not a vague priority: a small tenant's merge is admitted within a bounded number of scheduler cycles no matter how much a large tenant is pushing, exactly as ingest admission already guarantees a quiet tenant bounded latency under a loud one ([`interleave_by_tenant`](../crates/prism-engine/src/admission.rs), [ingestion contract §6](INGESTION-CONTRACT.md)). The scheduler reuses that round-robin discipline across tenants' pending merges, and the property test asserts the bound, not the intent.

**Write amplification per tenant is an observable counter**, not a hope — the review checklist has always demanded it and S10 makes it real: the scheduler accounts bytes-written ÷ bytes-read per tenant, so a tenant whose merges are amplifying can be seen doing it.

## 8. The gate is a soak, not a test

S10 is not proven by a unit test; it is proven by a **soak**:

> Sustained ingest **and** queries **and** deletes **and** a re-embed migration, running **concurrently**, with kill injection at merge boundaries throughout — and at hour *N* the store has a **steady-state part count**, **bounded merge debt**, **flat memory**, and — the assertion that matters — **recall and golden answers identical to hour 1**.

This is the S8/v1 72-hour recall-stability discipline, now with mutations underneath it. An **accelerated** soak runs in CI (compressed time, real concurrency and kills); the **full-length** soak runs nightly. The point is not that nothing breaks; it is that *the answer at hour N is the answer at hour 1* while everything underneath it churns.

## 9. Scope — size-tiered with budgets, proven indestructible, beats clever and fragile

S10 does **not** research compaction policy (no leveled-vs-tiered exploration) and does **not** build distributed merges ([S12](PRISM.md)'s problem). It builds **size-tiered merges with budgets** and proves them indestructible under sustained mutation and fault injection. A merge scheduler that is simple and cannot be made to corrupt or starve is worth more than a clever one that might.
