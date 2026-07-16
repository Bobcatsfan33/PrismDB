# The Storage & Cache Contract

**Status:** written in S11, before the object-store client — the discipline every other contract was written under. S11 moves the cold tier off the local disk and onto object storage, with a local cache in front of it, and that changes two things it must not be allowed to change silently: *when a part is durable enough to publish*, and *whether the layer of cache in front of a byte can change the answer that byte produces*. This contract pins both before the code.

The one fact it rests on, from S0: **a query answer is a function of the data, never of where the data physically lives.** A part on local disk, a part in object storage, a part half in a cache — these are three *physical layouts* of the same immutable bytes, and [C-4](DECISIONS.md)/[C-5](DECISIONS.md)/[C-7](DECISIONS.md) already forbid a physical layout from changing an answer. S11 adds a fourth member to that family (§3) and builds the storage layer so it holds.

---

## 1. Real backend, injected faults — never a hand-mocked object store

The object store is tested against **the real thing with faults injected into it**, never against a hand-written fake that returns canned responses — the S1 lesson (*test against the thing*), now at the storage boundary. The CI object store is **MinIO in a container**, fronted by a **fault-injecting proxy** that produces the failures a real remote store produces: latency spikes, 5xx, connection drops, **partial / truncated bodies**, and a slow first byte. A truncated or corrupted remote read gets the **named-byte error discipline** the local reader already has ([S1](PROGRESS.md)) — *"column X block N: needs K bytes, got J"* — never a generic failure and never a silent short read fed into a decoder.

> **Increment note (honest scope).** The engine talks to the store through one **`ObjectStore` trait** (§8). This sprint builds and gates the *backend-agnostic* semantics — durable-before-publish, CAS, cache-verify, fetch-budget, answer-invariance, degradation — against a **real local `ObjectStore` backend with the fault-injection wrapper**, which is a real object store (content-addressed keys → durable objects) exercised through real injected faults, not a mock of S3. The **MinIO container, the fault-injecting proxy, and the hand-rolled S3 client** ([D-065](DECISIONS.md)) that proves the S3-*protocol* specifics (SigV4, conditional-put wire semantics, multipart, XML errors) are the filed next increment, because they need container CI and a from-scratch HTTP/SigV4 client that is a sprint of its own. What ships is the door; the S3 wire is the sprint that needs the wire — the same honesty S7 (GPU-off), S8 (Flight transport deferred), and S9 (100M not claimed) shipped with.

## 2. Publication means remote-durable, and the catalog wins a race by CAS

[Invariant 2](PRISM.md) — *a catalog snapshot references only durable, checksum-valid parts* — is **extended to remote durability**:

> **A part's bytes are uploaded and verified before the catalog references it.** A snapshot must never point at bytes that are one disk failure away from gone.

- **Upload, then verify, then reference.** A part's objects are put to the store, and their presence and content hash are **verified** (a `head` confirming size, a content-hash check the store's own checksum agrees with) *before* the commit that names the part. Verify failure is a refused publication, not a published-and-hope.
- **Catalog publication is compare-and-swap.** The `CURRENT`-pointer swap that makes a snapshot live uses a **conditional put** — `If-None-Match` for a create, an `If-Match`/version-guard for a replace (the primitive and its MinIO/S3 semantics are documented in [D-066](DECISIONS.md)) — so two writers racing to publish **cannot both win**: one's conditional put succeeds, the other's fails the precondition and retries against the new state. Last-writer-wins on a shared pointer is a lost commit; CAS makes it a detected conflict.
- **Old-or-new across every upload boundary.** The kill-point matrix extends across each new boundary — **part object upload, a multipart part, the verify, the catalog CAS** — and each asserts the store lands old-or-new, **never hybrid**. A crash mid-upload leaves an object no snapshot names; a crash before the CAS leaves `CURRENT` at the old snapshot.
- **Orphans are GC's job, never publication's.** An upload that crashed, or a CAS that lost its race, leaves an orphaned remote object. It is reclaimed by **GC reconciling the remote listing against the referenced set** — exactly as local GC reclaims a `.tmp` orphan — and **never** by the publication path deleting as it goes. Publication that cleans up is publication that can delete a live object on a retry.

## 3. Answer-invariance, storage edition — a cache cannot change an answer

This is the **fourth member of the [D-033](DECISIONS.md) family** ([C-4](DECISIONS.md) layout, [C-5](DECISIONS.md) ISA, [C-7](DECISIONS.md) seed/scan-order, and now cache state):

> **Golden and layout-variant queries, run with the cold tier forced cold, forced hot, and mixed, return byte-identical answers.** Only the counters and the latency may differ.

A cache state — every block on the remote, every block resident, or any interleaving of the two — is a **physical layout**, and a physical layout is exactly what an answer may not depend on. The gate forces each cache state and asserts identical event ids in identical order. A cache that changes an answer is a bug of the same class as a layout-dependent tie-break; this contract makes it impossible to ship one quietly.

## 4. The cache trusts nothing, and it degrades by name

The cache is in front of the truth, so it is held to the truth's standard:

- **Every entry is verified by content hash on read.** A block served from the cache is checked against the checksum the part manifest already carries ([S1](PROGRESS.md)'s per-block CRC-32) exactly as a block read from the remote is. A cache hit is not a trust shortcut.
- **A corrupt entry is a named error, then a repair.** A cache block that fails its checksum produces a **named error**, is **evicted**, **refetched** from the remote, and the **repair is logged** — never served, never silently returned. The corruption is a fact about a disposable cache, not about the durable truth, so the query still succeeds against the refetched bytes.
- **A full cache disk gets the [S10](MERGE-CONTRACT.md) ENOSPC treatment.** Cache admission is bounded by a byte quota; a write that would exceed it triggers **eviction pressure**, and a genuinely full cache disk is a **named backpressure** condition (`OutOfSpace`, the S10 named error), never a wrong answer and never a hang. The cache is an optimization; running out of room to cache degrades performance, never correctness.
- **Remote unavailable is a named degradation, never a silent partial.** When the remote is unreachable, **cached data still serves** — a query answerable entirely from the cache succeeds — and a query that needs an uncached block **fails with the remote condition named** (`remote unavailable: <detail>`). It never returns a partial answer as if it were whole. This is the [S12](PRISM.md) slow/lost-shard rule — *documented partial behavior, never silent omission* — arriving one sprint early, because a cache miss against a dead remote is the same shape of failure.

## 5. New constants, the usual law

Cache size, cache admission, cache eviction, the ranged-read **coalescing gap**, and **prefetch depth** are tuned constants and obey [C-1](DECISIONS.md)/[C-3](DECISIONS.md): committed evidence or a rationale, policy bounds with a written reason measurement cannot see, and the boundary-artifact suspicion (an edge-of-grid optimum is a missing constraint). And a new axis: **every storage receipt is tagged engine- AND backend-conditional.** A byte count or a latency measured against MinIO-on-localhost is not the number for S3-over-a-WAN, and a receipt that does not say which backend it measured is a receipt nobody can use. `corpus_conditional` and `generation_conditional` gain a sibling: `backend`.

## 6. The rerank fetch budget is enforceable reality

D-003's economics — hot tier is PQ codes plus scalars, cold tier is exact vectors plus bodies — becomes a *bound*, not a description:

> **A plan declares its exact-vector fetch budget, and execution is bounded by it.** Fetching does not run unbounded; when the declared budget is exhausted mid-rerank, execution produces the **documented, named** degraded-or-refused behavior, never a silent over-fetch.

Until S11 the "budget" was a *width* (`rerank` count) with an implicit byte ceiling. S11 makes it a **byte budget** the plan carries and execution enforces: the rerank loop stops fetching when the budget is spent, and the result says so. And **`EXPLAIN` carries the cold-tier economics** — object requests, retrieved bytes, and an estimated cost per query — so the two-tier bill is a number on every query, not a whiteboard diagram. The declared budget and the actual spend both appear, the way estimates and actuals already do for the four query controls (query contract §14).

## 7. The cost worksheet is a receipt, not a slide

The number the company watches — **cost per billion events retained** — starts being a *measured output* here. A committed receipt records, for the reference workloads, the **measured bytes per million events in each tier**, the **hot/cold split**, and the **request counts** — so "cost per billion events" is arithmetic over measured bytes, not a projection. It is tagged with its backend (§5): the local-object-store number and the S3-over-WAN number are different products, and the worksheet says which one it is.

## 8. Scope — one backend, one region, one trait boundary

S11 targets **one backend (the S3 API), one region**. It does **not** do lifecycle-class tiering (Glacier and friends), a CDN, or any multi-cloud abstraction beyond the **`ObjectStore` trait** — one trait, one seam, and the backends live behind it. The client dependency question (a crate versus a hand-rolled client) is decided against the [D-002](DECISIONS.md) dependency charter and written down in [D-065](DECISIONS.md), because it is the first substantial external dependency on the **read path of the truth**, and *why it is trustworthy enough for that* is a decision that must be reviewable, not assumed.
