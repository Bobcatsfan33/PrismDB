# PrismDB — the semantic event store

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![CI](https://github.com/Bobcatsfan33/PrismDB/actions/workflows/ci.yml/badge.svg)](https://github.com/Bobcatsfan33/PrismDB/actions/workflows/ci.yml)

AI systems produce a new kind of telemetry — prompts, completions, tool calls, agent traces — and the questions that matter about it are questions of **meaning**. *Group today's two billion traces into behavioural patterns. Show me everything that resembles this failure. What behaviour exists this week that existed nowhere last week?* None of those are keyword searches, and none of them are `SUM(...) GROUP BY status_code`.

PrismDB answers them in SQL, at event scale, alongside ordinary filters on tenant, time, and cost — one engine, no glue code, and no sampling your data away. The rare event you threw out to afford the vector tier is the one the question was about.

## How it works

- **Immutable columnar parts, physically clustered by meaning.** Rows are stored in order of the cluster they belong to, so everything that means the same thing is already next to everything else that means the same thing. Similarity becomes a byte range instead of a graph traversal.
- **A tiny, always-resident centroid index.** A query scores a few thousand centroids, picks the handful worth looking at, and skips the rest of the dataset before touching a single row. It is the only index there is, and it fits in cache.
- **Compressed-vector scans at memory bandwidth, exact re-ranking of survivors.** Vectors are quantized down by ~32× and scanned as fixed-stride codes; only the bounded set of survivors ever has its full-precision vector fetched, and only that set decides the answer. Compression *is* the query accelerator: less memory traffic is less time.
- **Embedding models run inside the ingest path, versioned like schemas.** A model, a coarse codebook, and a quantizer codebook together form an immutable, content-addressed *generation*. Every part pins the generation it was written under, because a codebook defines what every stored byte means — so it is never edited, only superseded.
- **Background merges keep the clustering fresh under continuous streaming ingest.** One mechanism does compaction, deduplication, and model migration: read immutable parts, write new immutable parts, swap the catalog. Nothing is ever mutated in place, so a crash leaves an orphan rather than a hybrid, and a rollback is a catalog write rather than a restore.

## Quickstart

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"

# A synthetic corpus of agent telemetry, skewed the way real telemetry is skewed.
# (Or bring your own TSV: event_id, tenant_id, event_time, event_name, cost, error, body)
prism gen-corpus --kind zipf --rows 20000 --seed 42 --out events.tsv

# A store. dim/nlist/pq-m are the shape of the index; see docs/PRISM.md.
prism init --path ./demo --dim 64 --nlist 32 --pq-m 8

# Ingest: validate, embed, assign a centroid, quantize, write one immutable part,
# commit the catalog once. Anything unembeddable is dead-lettered, never stored blind.
prism ingest --path ./demo --file events.tsv

# Hybrid search: meaning AND scalar predicates, in one pass over one engine.
prism search --path ./demo \
  --query "the tool call timed out and we retried" \
  --tenant t1 --from 1760000000000 \
  --k 5 --nprobe 4

# Semantic GROUP BY: cluster whatever matched into behavioural motifs, each with
# a count, an average cost, an error rate, and a real exemplar event you can read.
prism search --path ./demo \
  --query "the agent failed" \
  --nprobe 16 --rerank 100 --group 5 --k 1

# Every search prints its physical-execution counters: parts pruned, ranges
# scanned, compressed bytes read, exact vectors fetched. Pruning is a number you
# can check, not a claim you have to take on faith.

# The exact oracle: brute-force every eligible row, no index at all. Slow on
# purpose. This is what the approximate path is measured against.
prism search --path ./demo --query "the agent failed" --k 5 --exact

# Housekeeping. GC is a separate, explicit operation and never runs inside a
# commit -- a reader holding a snapshot must never have the ground removed.
prism inspect --path ./demo
prism merge   --path ./demo
prism gc      --path ./demo --retain 5
prism verify  --path ./demo

# The offline validator: is this a part, and is it intact? Needs no catalog, no
# engine, no database standing up -- point it at a directory of bytes from a
# backup and it will condemn it, or clear it, and say which byte lied.
prism fsck --path ./demo
```

## Status

**Executable reference core under active development.** Sprints **S0 through S4** of eighteen are complete: a dependency-light, single-node vertical slice that really ingests, really prunes, really scans compressed codes, really re-ranks exactly, and really answers — on a hardened, versioned, self-describing storage format, behind an admission boundary with exactly-once replay semantics, with tenant isolation enforced as a physical property of which bytes a query is allowed to read. See [docs/PRISM.md](docs/PRISM.md) for the architecture and the full sprint roadmap, [docs/INGESTION-CONTRACT.md](docs/INGESTION-CONTRACT.md) for what an acknowledgement actually promises, [docs/QUERY-CONTRACT.md](docs/QUERY-CONTRACT.md) for what a cursor means, and [docs/PROGRESS.md](docs/PROGRESS.md) for exactly what is proven so far and by which test.

**What works today**

- Ingest → embed → normalize → coarse assignment → product quantization → immutable checksummed part → one atomic catalog commit.
- Hybrid query: metadata pruning, centroid probing, contiguous-range ADC scan with the scalar filter fused into the loop, a bounded candidate heap, exact re-rank inside a declared fetch budget, and semantic grouping with real exemplar events.
- **A storage format that refuses what it does not understand.** An explicit binary manifest with a format version, byte order, a feature bitset and per-column codec ids; column files framed into checksummed blocks so a flipped byte condemns one block and *names* it, not a whole column; and an offline validator (`prism fsck`) that condemns a suspicious part with no catalog, no engine, and no database standing up.
- **Nothing allocates on an untrusted length.** Every length in a part arrives from a stranger, and every one is checked against the bytes actually present before anything is reserved. The fuzz suite throws byte flips, every truncation length, total garbage, and a checksum-*repairing* adversary at the reader; it must decode or refuse, never panic.
- **The rerank tier is described, not assumed.** Each part declares its exact-vector encoding and the accuracy contract that encoding owes you, and the reader dispatches on it. Changing it later is a data migration, never a format break.
- **Old formats still open, and merge migrates them forward.** The v1 parts committed on day one still open, still verify, and still answer — and a merge rewrites them into v2 without ever touching the original bytes.
- **SQL — and it is provably the *same door*.** `SELECT … FROM events WHERE embedding ≈≈ 'a failure' AND attributes['gen_ai.system'] = 'anthropic' AND cost < 0.02 LIMIT 10` compiles to the same query the direct API takes and calls the same executor. Every gate test runs each query through *both* doors and asserts the rows **and the physical-execution counters** are identical — because if SQL ever grew its own scan, the counters would diverge before the results did.
- **Tenant policy is a shape, not a check.** The binder emits `(whatever you wrote) AND tenant_id = <your tenant>`. Your expression is a *subtree*, and a subtree cannot widen the conjunction it is nested inside — not with an `OR`, not a `NOT`, not an alias, not parentheses. There is no list of escapes to keep up to date, because there is nothing to escape *to*. Nineteen hand-written attempts and 8,000 fuzzed statements agree.
- **Pagination that cannot duplicate or drop.** A cursor pins a *snapshot*; paging continues to read that snapshot even while ingest and merge race underneath. A cursor into a reclaimed snapshot is an explicit error, never a silently different answer. No `OFFSET`. This needed no new invariant — only the ones we already had to be true.
- **Isolation is not a filter we promise to apply. It is a set of bytes we never read.** Rows are partitioned by `tenant-bucket × event-time window × generation`, and the partition key lives in the *catalog*, above the parts — so a part outside your partitions is never opened, never checksummed, never touched. The gate test does the strongest thing we could think of: it fills every other tenant's partitions with **unreadable garbage**, and every tenant-A query still answers correctly. *Because it never looked.* A pleasant consequence: damage is **attributable** — corrupt one tenant's compressed codes and that tenant's similarity search fails while their `COUNT(*)` keeps working, because a count does not read the codes. "Tenant bravo cannot run similarity search on this partition" is something an operator can act on. "The store is corrupt" is not.
- **A shared bucket hides its co-tenants from every query, and we tell you exactly what it does not hide.** Small tenants share physical parts, so part-level metadata — zone maps, attribute-key dictionaries — naturally describes the *bucket*, not the tenant. Ours is scoped **per tenant**: *"does this part contain key X?"* is answerable for you and about you, and a zone map is a zone map for one tenant, which closes the leak and also prunes better. What remains is [written down rather than pretended away](docs/QUERY-CONTRACT.md): an operator with **raw disk access** can see which tenants share a bucket. No query can. A dedicated bucket is the escape hatch — and a "dedicated" bucket found holding two tenants is *refused at commit*, because if it were accepted, every isolation claim resting on it would be false and nothing would notice.
- **A hot attribute can be promoted to a typed column — and it is the same door.** Promotion is a versioned, generation-like schema event, never an in-place rewrite, so promoted parts and mapped parts coexist and a merge migrates the old ones forward. The gate: the same query over a promoted key must return **identical rows and identical logical counters** whether it hits the column or the map — because if promotion changed what the engine *considered*, it would be a different query wearing the same text. The one counter allowed to differ is `physical_bytes_read`, and the test asserts it differs **downward**. That assertion caught a real bug the day it was written: the first implementation read *more* bytes than the map it replaced.
- Immutable content-addressed generations; a query spanning two embedding spaces is **refused**, not silently merged (scores from different spaces are not comparable).
- Merge with a documented duplicate policy, re-embed migration, catalog-only rollback, and explicit GC that provably never touches a referenced part.
- **An acknowledgement is a promise, and it is kept.** An acked event *will* become queryable — even if the process dies immediately afterwards, mid-embedding, before a single byte of its part is durable. It comes back **exactly once, with its embedding**, out of a durable admission log. Replays are recognised and suppressed; a reused id with *different* content is refused rather than silently rewriting history. Source offsets are advanced only *after* publication: they may lag reality, they may never lead it.
- **One tenant cannot starve another.** Quotas are enforced before a single GPU cycle is spent, and admission is round-robin across tenants — so a quiet tenant's latency does not change when a loud one gets a thousand times louder. That, not "the big tenant was throttled", is what the quiet tenant actually notices.
- **Attributes are bounded before they exist.** Caps on keys, key length, value length and total bytes — and, the only one that bounds the *shape* of the data rather than the size of an event, a bounded attribute-key dictionary per partition. A tenant emitting a uuid as an attribute *key* is refused and told why, rather than being quietly absorbed until the format dies of it.
- **Crash consistency, measured.** The writer is killed at every durability boundary, and then at 10,000 randomly chosen ones, and the store always opens to the old snapshot or the new one — never a hybrid.
- A recall contract measured against an exact brute-force oracle on a committed golden corpus — **reported with its tail**, not just its mean — and a machine-generated [`baselines.json`](baselines.json).

**Deliberate limits right now** — these are sprints, not oversights

- Single writer, single node, in-process. No network, no server, no distribution.
- SQL is a minimal subset: projections, filters, `LIMIT`, scalar aggregates and `GROUP BY`, plus the `embedding ≈≈ 'text'` predicate. No joins, no subqueries, no `OFFSET`. The full semantics — nulls, ties, model versions, the cost-based optimizer — are S8, and S8 may *extend* the query contract but not contradict it.
- Scalar loops only: no SIMD (S6), no GPU (S7). Every kernel here is the *reference* implementation that the fast ones will have to prove themselves equal to.
- A deterministic local hash embedder, not a real language model (S13). It exists so that every test, corpus, and baseline is reproducible on any machine with no weights and no network.
- Semantic grouping clusters the re-rank survivors. Grouping an arbitrarily large *filtered set* — the flagship aggregate — is S9.
- The probe count is fixed per query. Scaling it when a query sits on a cluster boundary is [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1), targeted at S6.
- **No network listener and no Kafka client.** The OTel GenAI *mapping* is real and tested (pinned to a semantic-convention version, because the conventions are still moving); the `Source` abstraction has exactly Kafka's offset semantics and the file-backed source implements them, so invariant 7 is tested through real process deaths. The server is S14. The gate here was the semantics, not the transport.
- **Every tuned constant here was derived on a hash-embedder corpus, and is marked `corpus_conditional` in the ledger because of it.** The hash embedder makes tests reproducible with no weights and no network — and its motifs are unusually well-separated, which is exactly the wrong property in a corpus you tune an index on. Building a real-embedding golden corpus and re-deriving every sweep against it is [issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3). Honest is not the same as fixed.
- A shared bucket's *manifest bytes* still name its co-tenants to anyone with raw disk access. Per-tenant envelope encryption is S14; a dedicated bucket is the answer until then. Stated in the [query contract](docs/QUERY-CONTRACT.md), not discovered by a customer.

**Numbers.** There are none in this README on purpose. Every performance claim PrismDB makes must be backed by a committed, reproducible benchmark artifact, and a roofline must be labelled a roofline. Run `prism bench --out baselines.json` and read what your own hardware says.

**And no golden corpus that moves.** The corpus every receipt is measured against is a *frozen, versioned, checksummed* artifact. A drift check compares committed bytes; it never regenerates what it is checking. We learned that the hard way: in S2 a change to the corpus *generator* silently changed the corpus, and the fixture script regenerated both the corpus and its expected answers — so the drift check would have gone on passing **by construction while testing nothing**.

**And no tuned constant without evidence.** Every constant that steers behaviour is in a [committed ledger](testing/evidence/registry.json), classified: a *tuned* constant owes a benchmark artifact, the key inside it that **is** the value, and the rule by which that rule chose it — and a test asserts, in both directions, that the code and the ledger still agree. A *policy* constant owes a written argument instead, because some questions measurement cannot answer. The first thing that rule caught was our own: the block size had been set to 64 KiB in S1 because 64 KiB is what people set it to, and measuring it showed a **247× read amplification**. The derived answer is 4 KiB, and queries got 2.2× faster.

**One number we will show you anyway, because it is a warning and not a boast.** With one centroid probed, PrismDB answers *topic* queries — aimed at the middle of a cluster — with a mean recall of 1.000. Across the whole golden set the mean is 0.904, which sounds like a good day. It is not: five of those queries return **nothing at all**. Their neighbours sat on a boundary between two clusters and we only looked in one. That is why every recall report here carries `min`, `p1`, `p5` and a count of queries that came back empty; why the golden corpus deliberately asks questions that straddle boundaries; and why the default probe count is *derived* from that tail with a [committed receipt](testing/golden/nprobe-provenance.json) rather than picked because it looked reasonable.

## Contributing

Read [docs/PRISM.md](docs/PRISM.md) Part II first — the engineering charter and the ten consistency invariants are not style preferences, and a change that violates one will be rejected however good it is otherwise. In short: immutability is law; GC never runs in the publish path; every SIMD or GPU kernel needs a scalar twin that CI proves it equal to; approximation is always measured against an exact oracle; and every ticket names the invariant it preserves, the metric it moves, and the test that proves both.

Then see [docs/PROGRESS.md](docs/PROGRESS.md) for the next open sprint gate and [docs/DECISIONS.md](docs/DECISIONS.md) for the judgment calls already made.

## License

Apache-2.0. Permanently, and without exception — see [LICENSE](LICENSE).
