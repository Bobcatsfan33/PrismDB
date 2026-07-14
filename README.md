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
```

## Status

**Executable reference core under active development.** Sprint S0 of eighteen is complete: a dependency-light, single-node vertical slice that really ingests, really prunes, really scans compressed codes, really re-ranks exactly, and really answers. See [docs/PRISM.md](docs/PRISM.md) for the architecture and the full sprint roadmap, and [docs/PROGRESS.md](docs/PROGRESS.md) for exactly what is proven so far and by which test.

**What works today**

- Ingest → embed → normalize → coarse assignment → product quantization → immutable checksummed part → one atomic catalog commit.
- Hybrid query: metadata pruning, centroid probing, contiguous-range ADC scan with the scalar filter fused into the loop, a bounded candidate heap, exact re-rank inside a declared fetch budget, and semantic grouping with real exemplar events.
- Immutable content-addressed generations; a query spanning two embedding spaces is **refused**, not silently merged (scores from different spaces are not comparable).
- Merge with a documented duplicate policy, re-embed migration, catalog-only rollback, and explicit GC that provably never touches a referenced part.
- Crash consistency: the writer is killed at every durability boundary in CI, and the store always opens to the old snapshot or the new one — never a hybrid.
- A recall contract measured against an exact brute-force oracle on a committed golden corpus, and a machine-generated [`baselines.json`](baselines.json).

**Deliberate limits right now** — these are sprints, not oversights

- Single writer, single node, in-process. No network, no server, no distribution.
- No SQL yet (S3). The CLI is the whole surface.
- Scalar loops only: no SIMD (S6), no GPU (S7). Every kernel here is the *reference* implementation that the fast ones will have to prove themselves equal to.
- A deterministic local hash embedder, not a real language model (S13). It exists so that every test, corpus, and baseline is reproducible on any machine with no weights and no network.
- Semantic grouping clusters the re-rank survivors. Grouping an arbitrarily large *filtered set* — the flagship aggregate — is S9.
- Tenant isolation is a filter fused into the scan, not yet a physical partition boundary (S4).

**Numbers.** There are none in this README on purpose. Every performance claim PrismDB makes must be backed by a committed, reproducible benchmark artifact, and a roofline must be labelled a roofline. Run `prism bench --out baselines.json` and read what your own hardware says.

## Contributing

Read [docs/PRISM.md](docs/PRISM.md) Part II first — the engineering charter and the ten consistency invariants are not style preferences, and a change that violates one will be rejected however good it is otherwise. In short: immutability is law; GC never runs in the publish path; every SIMD or GPU kernel needs a scalar twin that CI proves it equal to; approximation is always measured against an exact oracle; and every ticket names the invariant it preserves, the metric it moves, and the test that proves both.

Then see [docs/PROGRESS.md](docs/PROGRESS.md) for the next open sprint gate and [docs/DECISIONS.md](docs/DECISIONS.md) for the judgment calls already made.

## License

Apache-2.0. Permanently, and without exception — see [LICENSE](LICENSE).
