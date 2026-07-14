# The Generation Contract

**Status:** written in S5, *before* the lifecycle was built — the same discipline the [ingestion contract](INGESTION-CONTRACT.md) and the [query contract](QUERY-CONTRACT.md) were written under. A migration is the one operation that can silently change what every byte in the store *means*, so what it promises is pinned here first and the code is written against it.

> **A codebook defines the meaning of every byte encoded under it.** That is the whole reason generations exist, and the reason none of this is negotiable.

Where this contract and the code disagree, **the contract is right and the code is a bug.**

---

## 1. What a generation is

```
generation = (model_id, model_version, dim, coarse codebook, PQ codebook)
```

Content-addressed: the id **is** the SHA-256 of that tuple. Immutable, like everything else here. Every part pins exactly one generation id, and a part without a resolvable generation is not readable — not "readable with a warning", not "readable on a best-effort basis". **Unreadable.**

Two generations may share a `(model_id, model_version)` — an **embedding space** — and differ only in their codebooks. This distinction carries the whole migration story, so it is worth being exact about:

| | same space | different space |
|---|---|---|
| what changed | the codebooks (coarse / PQ) | the model or its version |
| are exact scores comparable? | **yes** — the same vector means the same thing | **no** — different geometry, different units |
| how a query spans them | one ADC table per generation, merged at **exact-score** time | **refused**, unless a bridge is declared (§6) |

The PQ code is an *approximation device*; the exact vector is the *answer*. Two generations in one space disagree about how to approximate, never about what a vector means — so they merge at the exact-score step, where both agree.

## 2. Codebooks are trained on a stratified sample of *logical rows*, never on the first batch

> *"codebooks trained from stratified/reservoir samples (never just the first batch)"* — PRISM.md, S5

A codebook trained on the first batch bakes that batch's distribution into the meaning of every byte written afterwards. A codebook trained on *the rows that happened to be scanned first* is worse, because it is not even a statement about the data.

**The training sample is selected by `event_id`, not by position** ([charter C-4](DECISIONS.md)). A reservoir keyed on the row's identity picks the same rows no matter which part they live in, which order the parts are listed, or how the ingest was batched. Anything else means two stores with identical rows train **different codebooks** — and a generation that depends on the layout is a store whose bytes mean something different after a merge.

Sampling is **stratified over partitions** (tenant-bucket × time window), proportionally allocated, with a floor per stratum. A store whose loudest tenant emits 100× the rows of everyone else would otherwise get a codebook that describes that one tenant, and every other tenant's recall would quietly pay for it.

**Every sample records its provenance** — the strategy, the strata, the counts, the seed, the snapshot it was drawn from — in the generation record, like everything else here.

**The bootstrap generation is the one honest exception, and it is marked as such.** The very first ingest has to encode its rows under *some* codebook, and there is nothing to stratify over yet. It is trained on a reservoir of that first batch, recorded with `provisional: true`, and it is exactly what the lifecycle in §3 exists to replace. A provisional generation is not a bug; a provisional generation *nobody told you about* is.

## 3. The lifecycle

Every transition is a **catalog commit** — an atomic snapshot swap, and nothing else. There is no state living in a writer's memory, no half-migrated flag, no repair path. Which is also why rollback is trivial (§5).

```
create ──▶ canary ──▶ compare ──▶ promote ──▶ migrate ──▶ complete ──▶ retire
   │                                  │                        │
   └──────────── rollback ◀───────────┴────────────────────────┘
```

- **create** — train a new generation from a stratified sample (§2). It is registered as a **candidate**: no part is encoded under it, no query uses it, nothing has changed. Creating a generation is free and reversible because it does nothing.
- **canary** — re-embed a *bounded* set of partitions into the candidate. The store now has **two live generations**, and that mixed window is a normal operating state, not an incident. Queries keep working throughout (§4).
- **compare** — run the frozen probe queries under both generations and emit a receipt: agreement, recall against the exact oracle, scan cost. A promotion without a comparison is a hope.
- **promote** — the candidate becomes **active**: new writes encode under it. Existing parts are untouched, and keep answering under their own generations.
- **migrate** — re-embed the remaining parts, partition by partition, resumable.
- **complete** — see §7. It is not what you think it is.
- **retire** — a generation record may be dropped only when **no part in any retained snapshot references it**. Retiring a generation a snapshot still names would make that snapshot unreadable, which is the one thing a rollback target may never be.

## 4. Queries keep working throughout the migration

**This is the gate.** A store in the middle of a two-generation migration answers every query it could answer before.

- Each generation brings its own ADC table; a query scans parts of both and merges at exact-score time.
- **No part is ever decoded with the wrong codebook.** A part names its generation; the reader resolves it; a mismatch is an error, never a decode. This is the property that fault injection and the property tests exist to prove, because the failure mode is not a crash — it is a *plausible wrong answer*, which is far worse.
- The four query controls, the total order, and the pagination semantics do not change during a migration. A cursor pins a snapshot (query contract §2), and a snapshot pins its generations, so a paginating reader is *unaffected* by a promotion that happens underneath them.

## 5. Rollback is a catalog reference change, never a data rewrite

Parts are immutable, so the old generation's parts are still sitting there, byte-identical. Rolling back is committing a snapshot that names them. It cannot fail halfway, because it is one atomic rename.

**GC is what makes this true, and GC is not in the publish path.** A generation's parts survive until GC is *separately* asked to reclaim what no retained snapshot names. Retire (§3) is the only thing that makes a rollback impossible, and it is deliberately the last step.

## 6. Scores from different embedding spaces never merge without a declared bridge

> *"Scores from different embedding spaces are never merged without an explicit, validated bridge policy."* — PRISM.md, Part II, invariant 9

A cross-space query is **refused** by default, naming the spaces it found. That refusal is the correct behaviour and the common case: a cosine of 0.83 in one model's space and 0.83 in another's are two different numbers that happen to be printed the same way, and averaging them is not a merge, it is a category error with a plausible-looking result.

A **bridge** is a catalog-registered declaration that says how — and *whether* — two spaces may be answered together. It is explicit (someone declared it), validated (it carries a receipt), and named in the query's output, so a bridged answer can never be mistaken for a native one.

**The only implemented bridge policy is `rank_fusion`, and it does not merge scores at all.** It merges *ranks*: each space ranks its own rows, in its own units, against its own query embedding, and the ranks — which are unitless — are fused. This obeys the invariant rather than working around it. A bridge that averaged scores would be forbidden by this document even if someone implemented it.

A bridged result is **labelled** in its counters and its output. Silence about a bridge would make it a lie.

## 7. Completeness — the definition, and why it is bigger than it looks

The obvious definition is: *the migration is complete when no active part references the old generation.* That is **necessary and not sufficient**, and shipping only that would break drift detection silently.

**A drift baseline is a statement about a distribution in a particular embedding space.** A novelty score is a distance from that baseline. When the space changes underneath, the baseline is not *stale* — it is **meaningless**, and invariant 9 forbids comparing across it. An alarm that keeps evaluating against a baseline from the old generation would keep producing numbers, and the numbers would be nonsense, and nobody would be told.

> **A re-embed migration is complete only when every drift baseline has been recomputed under the new generation.**

The inputs for that recomputation are exactly what the migration already produces: re-embedded historical parts. It is not extra work; it is work the migration was already doing, and it is not finished until it has been used.

**During the mixed window, each generation is evaluated against its own baseline.** `NOVELTY` and `SEMANTIC_DIFF` over a generation's events use *that generation's* baseline and no other. Never cross-generation. Not "close enough". Not "the old baseline until the new one is ready".

### When a baseline cannot be recomputed

Recomputing a baseline requires re-embedding the historical rows, and re-embedding requires their **raw bodies** — which are retention-controlled and expire, by design, because prompts contain secrets.

So it will happen: the rows are still there, the bodies are gone, and the baseline **cannot** be rebuilt in the new space.

> **The affected alarm enters an explicit `DEGRADED` state and says so, loudly, on every evaluation. An alarm is never silently absent.**

A drift alarm that quietly stops firing is worse than one that was never configured, because a configured alarm is *trusted*. `DEGRADED` names the baseline, the generation, and the reason. It does not return zero. It does not return the old numbers. It does not return nothing.

---

## 8. What a receipt is conditional on

C-3 established that a constant derived on a hash-embedder corpus is `corpus_conditional`. **A new codebook generation changes the PQ geometry**, so the C-1 constants (`nprobe`, candidate width, rerank width) may not transfer across it either.

Every receipt records **the generation it was measured under**, and every tuned constant is `generation_conditional`. Re-sweeping on a new generation is the same standing obligation as re-sweeping on a real-embedding corpus ([issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3)), and for the same reason: a receipt that describes an engine which no longer exists is not evidence, it is decoration.
