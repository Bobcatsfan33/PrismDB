# The Query Contract

**Status:** written in S3, *before* the SQL surface was exposed to anyone. Pagination is the part of a query API you cannot change later without breaking every client that ever used it, so it is pinned here first and the code is written against it.

> **S8 may EXTEND these semantics. It may not contradict them.**
>
> That sentence is the whole point of this document. S8 owns the full SQL semantics — nulls, ties, ordering, model-version behaviour, the cost-based optimizer. It will add to what is written here. It will not redefine it, because by then there will be design partners with cursors in their code, and a cursor whose meaning changed is worse than a cursor that never worked.

Where this contract and the code disagree, **the contract is right and the code is a bug.**

---

## 1. Every result has a deterministic total order

```
ORDER BY score DESC, event_id ASC
```

**Always. Ties break on `event_id`, without exception.**

`score` is the exact cosine against the stored vector. For a query with no semantic predicate, every row has the same score and the order collapses to `event_id ASC` — which is still a *total* order, and that is what matters.

**And the tie-break binds the *selection*, not just the sort.** The candidate heap is bounded, so it decides which tied rows are *allowed to be* answers at all. If it breaks distance ties on physical position, then two stores holding identical rows answer the same query differently, and a merge changes an unchanged answer — while the final sort looks impeccable. S4 shipped exactly that bug and the recall floor caught it ([D-033](DECISIONS.md)). Ordering the output correctly is not enough if the wrong rows were chosen to order.

The tie-break is not cosmetic. Real telemetry repeats bodies verbatim, so exact-score ties are common. S0 broke ties on physical position — which part a row happened to live in — and a merge that moved rows between parts changed the order of an unchanged answer ([D-008](DECISIONS.md)). **Order must be a function of the data, never of the layout.** A pagination scheme built on an order that the storage engine can quietly permute is a pagination scheme that duplicates and drops rows for reasons no one will ever debug.

## 2. A cursor is an opaque token binding a snapshot and a position

```
{ catalog snapshot, position in the total order }
```

- **Opaque.** It is a token, not a structure. Clients do not parse it, construct it, or arithmetic on it. It is currently a checksummed hex-encoded blob; it may become anything, and no client may notice.
- **Bound to a snapshot.** A cursor names the catalog snapshot the query was answered from, and paging continues to read **that** snapshot — not `CURRENT`. Readers pin a snapshot for the lifetime of a query (invariant 4), and a paginated query is a query whose lifetime spans several requests.
- **Bound to a plan.** The cursor also carries a hash of the query and its four controls. Presenting a cursor from one query to a different query is an error, not a surprising result.

### The snapshot is what makes pagination correct

Parts are immutable and a snapshot is a fixed set of them. So the answer to a query *against a given snapshot* is fixed, forever, no matter what else is happening:

- **Ingest** publishes a new snapshot. The old one is unchanged, and the paginating reader does not see the new rows. Its pages remain exactly the rows of the snapshot it started on.
- **Merge** rewrites parts into new parts and publishes a new snapshot. The old parts are *untouched* — immutability is law — so the old snapshot still resolves, and the answer is byte-identical.
- **GC** reclaims only what no *retained* snapshot names.

That is the entire mechanism. Pagination did not need a new invariant; it needed the ones we already had to be true.

### An expired snapshot is an error, never a different answer

If the cursor's snapshot has been reclaimed by GC, the query **fails, loudly**, naming the snapshot and saying so:

```
cursor is bound to snapshot s00000007, which has been reclaimed;
re-run the query to start from the current snapshot
```

It does **not** silently continue against `CURRENT`. Silently switching snapshots mid-pagination is how a client receives a page that overlaps the last one, or skips rows that existed the whole time, and concludes the database is lying to them. It is. **A stale cursor is a caller's problem, and they can only solve it if we tell them.**

## 3. No OFFSET

`OFFSET n` is not supported and will not be.

`OFFSET` means "compute the first *n* rows and throw them away". It gets slower the deeper you page, and — worse — it is *wrong* against a moving dataset in exactly the way clients never anticipate: rows inserted before your offset shift everything, so page 2 re-shows rows from page 1, and rows are skipped entirely.

Keyset pagination on `(score DESC, event_id ASC)` against a **pinned snapshot** has neither problem. It costs the same at page 1,000 as at page 1, and it cannot duplicate or drop.

## 4. What the result set *is*

**The result set of a paginated query is the set of rows the plan produced** — that is, the plan's re-rank survivors, in the total order of §1.

This is stated plainly because it is a real limit and it will surprise someone. A semantic query is executed under four declared controls (`nprobe`, candidate width, rerank width, `LIMIT`), and the rerank width **is** the depth of the result set. With `rerank = 50`, there are 50 rows to page through, and the sixth page of ten is the end of the data — not the end of the *matching* data, the end of what this plan was asked to produce.

That is not a bug to be papered over. It is the honest consequence of a bounded-cost query, and the alternative — silently widening the plan when a client pages — would mean a query's cost depends on how many times you ask for it.

**And it is why `DEFAULT_RERANK` has a floor.** The joint sweep of the two widths ([`testing/evidence/widths.json`](../testing/evidence/widths.json)) would otherwise have chosen `rerank = 10`, because on the golden corpus the recall floors do not bind at all — every point in the grid clears them. A rerank width of 10 with a page size of 10 makes the first page the entire result set and the cursor decorative. So `MIN_PAGEABLE_ROWS = 50` is a *policy* bound on the derivation: the default plan must serve at least five pages. Measurement could not see that. Prose can.

Aggregate queries (`GROUP BY`) are not paginated. A cursor presented with `GROUP BY` is an error.

## 5. The SQL surface is the same door, not a second one

The SQL layer **compiles to the same `Query` the direct API takes, and calls the same executor.** It is a parser and a binder; it is not a second implementation of anything.

This is enforced, not asserted: every gate test runs each query through **both** the direct path and SQL, and asserts the results are byte-identical *and that the physical-execution counters are identical too*. If SQL ever grows its own scan, its own pruning, or its own idea of ordering, the counters diverge before the results do, and the test fails on the counters first.

Two doors into a database that disagree is a category of bug that takes years to find, because each door is individually self-consistent.

## 6. Tenant policy is injected below SQL

> *"mandatory tenant policy injected by the authorization layer (not removable by SQL)"* — PRISM.md, Part III §11

The session's tenant is **not** a SQL-level predicate. It is applied by the binder, beneath the user's `WHERE` clause, as:

```
(whatever the user wrote)  AND  tenant_id = <session tenant>
```

The user's expression is a *subtree*. Nothing inside it — an `OR`, a negation, an alias, a cast, a rewritten comparison — can widen the conjunction it is nested inside. A user may narrow their own visibility (`WHERE tenant_id = 'nobody'` returns nothing); they cannot broaden it.

`SELECT tenant_id` is allowed. Constraining it is allowed. *Escaping* it is not expressible, and the fuzzer spends its time trying.

## 7. The parser is network-facing input

The SQL text is now the same category of thing as a part file: bytes from a stranger. S1's discipline applies, in full — **nothing allocates on an untrusted length**, and every bound is named in its error:

| bound | limit |
|---|---|
| statement bytes | 64 KiB |
| tokens | 4,096 |
| expression nesting depth | 32 |
| `IN` list length | 1,024 |
| projections | 64 |
| `GROUP BY` keys | 16 |

A statement that exceeds one is refused with a specific error naming the bound and the value. The parser must **never** panic, never recurse without a depth counter, and never reserve capacity on the strength of a number it just read.

---

## 8. Tenant isolation, and the shared-bucket seam

Isolation is not a filter we promise to apply. **It is a set of bytes we never read.**

Rows are partitioned by `tenant-bucket × event-time window × semantic generation`, and the partition key lives in the **catalog** — above the parts. A query for tenant A never opens a part outside A's partitions: not to check it, not to prune it, not at all.

That is testable, and it is tested in the strongest form we could think of: **fill every other tenant's partitions with unreadable garbage, and tenant A's queries still answer correctly — because they never looked.**

A useful consequence: damage is **attributable**. Corrupting one tenant's part affects that tenant, and — because a column is only read if a query needs it — only the queries that actually touch the damaged column. "Tenant bravo cannot run similarity search on this partition" is something an operator can act on. "The store is corrupt" is not.

### Buckets: shared and dedicated

Large tenants get a **dedicated bucket** and share a part with nobody. Small tenants are hashed (with SHA-256 — a tenant must not be able to *choose* their bucket by choosing their id) into **shared buckets**, because a store with ten thousand tenants cannot afford ten thousand sets of partitions.

### What a shared bucket does and does not hide

In a shared bucket, part-level metadata naturally describes the *bucket*, not the tenant. So the metadata a **query** can observe is scoped per tenant: every part carries a per-tenant section, and a query reads its own and no other.

- *"Does this part contain attribute key X?"* is answered **per tenant**.
- A **zone map is a zone map for one tenant** — which closes the leak and also prunes better.
- No row, count, error, or execution counter reveals another tenant's data.

**What is not hidden, stated plainly:** the union attribute-key dictionary and the tenant list are in the manifest bytes, because the dictionary is what *decodes* the attribute column and the tenant list is what *prunes* the part.

> **An operator with raw disk access to a shared bucket can see which tenants share it, and the union of their attribute keys. No query can.**

If that is unacceptable for a given tenant, they get a **dedicated bucket** — and a dedicated bucket holding two tenants is *refused at commit*, because if it were accepted, every isolation claim resting on dedicated buckets would be false and nothing would notice. S14's envelope encryption closes the disk layer properly.

This is a deliberate, logged decision ([D-030](DECISIONS.md)), not an oversight. A seam you have written down is a seam you can close later; a seam you have not is a breach you find out about from someone else.

---

# The semantics edition (S8)

**Status:** S8 owns the full query semantics the S3 contract deferred — nulls, ties, ordering, threshold-vs-top-k, and model-version/generation selection. Everything below **extends** §1–§8; per §5's binding sentence, *S8 may extend these semantics, it may not contradict them.* Every S8 gate test cites the clause it exercises.

## 9. Plan-invariance — the physical strategy is invisible to the answer

A semantic query can be executed three ways — **scalar-first** (filter, then distance the survivors), **semantic-first** (distance the probed rows, then filter), and **interleaved** (the fused scan) — and they are three *physical strategies for one logical query*. This is [D-033](DECISIONS.md) in its plan edition, the sibling of the route's [selection-identity](DETERMINISM-CONTRACT.md):

> **The chosen plan may cost differently; it may not answer differently.** Every strategy returns byte-identical event ids in byte-identical order.

It holds *by construction*: all three compute the **identical candidate set** — the top-`candidates` predicate-satisfying rows by PQ distance among the `nprobe` probed centroids — and then rerank it identically. They differ only in *when* the predicate is evaluated relative to the distance, which changes the work done, never the set produced. The tie-break that makes this exact is C-4's, now stated at the SQL level (§11).

So, once proven, **a cursor need not pin the plan** (unlike the route, which pins because its *scores* differ — §D-052). The plan changes no score, so a page-2 keyset boundary is identical whichever strategy computed it. The gate proves it: paginate while forcing the plan to flip between pages, and the pages tile the answer exactly.

## 10. NULL semantics

A row's attribute is **absent** (the key was never written) or **present**. There is no stored SQL `NULL` distinct from absence — an unwritten attribute *is* the absence.

- A predicate on an absent attribute is **false**, never NULL-propagating. `attributes['x'] = 'y'` on a row without `x` does not match; `attributes['x'] != 'y'` on a row without `x` also does not match. Absence satisfies no comparison — a row must *have* the attribute to be compared. This is deliberately **not** three-valued logic: SQL's `NULL`-propagation is a footgun in a filter that decides tenant visibility, and a two-valued "absent matches nothing" is the safe reading.
- `AND` / `OR` / `NOT` are ordinary two-valued boolean operators over that base. `NOT (absent = 'y')` is `NOT false` = `true` **only if the row is otherwise in scope** — but since the comparison is false, its negation is true, so `WHERE attributes['x'] != 'y'` returns rows that *have* `x` unequal to `y` **and** rows that lack `x`. A caller who means "has x, and it isn't y" must say `attributes['x'] IS PRESENT AND attributes['x'] != 'y'`.
- **This is a place the two-valued choice surprises people, so it is stated loudly rather than discovered.**

## 11. Tie semantics — C-4, at the SQL level

The total order is §1's, and it is now stated as a SQL guarantee: **`ORDER BY score DESC, event_id ASC`, always, ties broken on `event_id`.** A query may not override it with its own `ORDER BY` (S8's SQL still refuses one, per S3); the order is the contract's, because pagination and plan-invariance both rest on it. Exact-score ties are common — real telemetry repeats bodies — and they break on the row's *identity*, never on its physical position or on which plan or route computed it. This is [charter C-4](DECISIONS.md) surfacing in SQL: the answer is a function of the data.

## 12. Threshold and top-k interact, and the interaction is defined

A semantic query may carry a **similarity threshold** (return only rows scoring above `τ`) and a **top-k** (`LIMIT`). When both are present:

> The result is **the top-k of the rows that clear the threshold** — threshold first, then `LIMIT`, in that order.

A threshold that admits fewer than `k` rows returns fewer than `k`; that is not an error, it is the honest count of rows that met the bar. A threshold that admits more than `k` returns exactly `k`, the nearest. The threshold is applied to the **exact rerank score**, never the PQ distance — the approximate distance decides who is reranked, and a threshold on an approximate score would admit rows the exact score rejects. `LIMIT` without a threshold is the S0 behaviour, unchanged.

## 13. Model-version / generation selection, and the error that teaches

A store mid-migration holds parts in more than one generation, and possibly more than one embedding **space** (§ invariant 9). SQL selection semantics:

- A query with no space named searches the **active** generation's space. Parts in a deprecated generation of the *same space* are included — their scores are comparable — and their per-generation ADC tables merge at exact-score time.
- Parts in a **different embedding space** are never silently merged. A query that would span two spaces is **refused**, and the error names both spaces and the fix. The message is written to *teach*, because this is invariant 9 surfacing where a SQL user will first meet it:

  ```
  this query spans two embedding spaces — hash-embedder:v1 and hash-embedder:v2 —
  whose scores are not comparable (a cosine of 0.8 in one is not a cosine of 0.8 in
  the other). PrismDB will not merge them into one ranking. Either name one space with
  `USING SPACE 'hash-embedder:v2'`, declare a bridge to fuse their ranks
  (`prism bridge declare`), or finish the re-embed migration so a single space remains.
  ```

- A **bridge** ([D-039](DECISIONS.md)) makes a cross-space query answerable by rank fusion, and a bridged SQL result is labelled as such in its `EXPLAIN`, never mistakable for a native ranking.

## 14. EXPLAIN carries estimates *and* actuals

`EXPLAIN` reports, for all four controls (`nprobe`, candidate width, rerank width, `k`) and for bytes / parts / ranges: the optimizer's **estimate** and the query's **actual**, plus the chosen route and plan **with the reason** — the receipt id and the threshold that decided it. The estimate-vs-actual error is tracked across the selectivity matrix in CI (the calibration harness), so cost-model drift is a visible number, not a slow surprise. An optimizer that cannot say *why* it chose a plan is an optimizer nobody can debug.

# The semantic-aggregate edition (S9)

**Status:** S9 owns `GROUP BY semantic_cluster(embedding, k)` — grouping by *meaning* over an arbitrarily large filtered set — and the two primitives `NOVELTY(embedding) AGAINST (baseline)` and `SEMANTIC_DIFF(a, b, k)`. Everything below **extends** §1–§14 and may not contradict them; the determinism of the clustering itself is the [determinism contract §13–§15](DETERMINISM-CONTRACT.md). Every S9 gate test cites the clause it exercises.

The semantics below are **built and gated at the engine level** (`Engine::semantic_cluster`, `Engine::novelty_against`, `Engine::semantic_diff`), where correctness lives. The SQL *keyword* grammar that spells them (`GROUP BY semantic_cluster(...)`, `NOVELTY ... AGAINST`, `SEMANTIC_DIFF`) is the next increment — deferred exactly as S8 deferred the Flight wire transport, and for the same reason: the semantics ship first and are proven, the surface that types them follows. The clauses here are written in that grammar because the grammar is the destination; they bind the semantics regardless of which door reaches them.

## 15. A cluster id is query-scoped and ephemeral; the stable output is the exemplar and the stats

A `semantic_cluster` result is *k* groups, and each carries a small integer `cluster_id`. **That id is scoped to the one query that produced it and means nothing outside it.** It is not a stable identifier for "the errors cluster" across two queries, two days, or two `k` values — cluster the same rows tomorrow with `k+1` and every id may land on different meaning. Treating it as stable is the mistake §2 warns about for cursors, one level up.

> The **stable, comparable** output of a cluster is its **exemplar** (a real `event_id`, §16's most-central event) and its **statistics** (count, `avg(cost)`, `countIf(error)`, …). Those are functions of the data. Two queries that want to be compared compare exemplars and stats, never raw cluster ids.

This is why the ids are assigned *last*, deterministically, from the group order (§16): they are a presentation convenience, not an identity. A caller who needs a durable label builds it from the exemplar's `event_id`.

## 16. Group ordering is deterministic — size, then exemplar identity

The groups of a `semantic_cluster` result are returned in a total order, fixed by the contract, never by hash-map iteration or cluster-fit accident:

> **`ORDER BY count DESC, exemplar.event_id ASC`.** Largest cluster first; ties in size broken on the exemplar's `event_id` ascending.

The size-descending rule puts the mass where a reader looks first; the `event_id` tie-break is [charter C-4](DECISIONS.md) again — when two clusters are the same size, the one whose exemplar has the smaller `event_id` comes first, so the order is a function of the data and identical across every layout, plan, and route. The ephemeral `cluster_id` (§15) is then just this order's index. `LIMIT` over a `semantic_cluster` result takes the first *n* groups *in this order* — the *n* biggest clusters — which is well-defined precisely because the order is.

## 17. The aggregate is bounded before it runs — `k` and clustering state

A `semantic_cluster` over a billion-row filtered set must not be a way to make the node allocate a billion rows' worth of clustering state. So the aggregate is bounded the way S2 bounded attributes — *before* the state exists, not after it OOMs:

- **`k` is capped by policy** at `MAX_SEMANTIC_K` ([C-3](DECISIONS.md) policy bound, with its rationale in the registry). A query asking for more clusters than a human can read is refused with a **named limit**, not silently clamped — clamping would answer a different question than the one asked.
- **Clustering state is budgeted and admission-controlled.** The state is `k` centroids plus per-cluster running aggregates plus a bounded exemplar selection; its size is `k`-bounded, *not* row-bounded (the rows stream through in mini-batches — §13 — and are never all resident as vectors). A query whose declared `k` and dimension would exceed `SEMANTIC_STATE_BUDGET_BYTES` is refused with a named limit **before** the first batch is read. A `semantic_cluster` never OOMs the node; it declines, the way ingest declines a tenant over quota (S2).

## 18. `NOVELTY ... AGAINST` names its baseline, and a space mismatch is the invariant-9 error

`NOVELTY(embedding) AGAINST (baseline)` scores each row by its distance to the nearest centroid of a **baseline snapshot** — a frozen, generation-scoped drift baseline ([D-038](DECISIONS.md)). `SEMANTIC_DIFF(a, b, k)` reports the clusters with mass in *b* and none in *a*. Both compare a row to *known structure*, and known structure lives in exactly one embedding space, so:

- The clause **names its baseline generation explicitly**, or **defaults to the query's own generation.** A baseline is generation-scoped because a centroid is only a distance from vectors in its own space; a baseline built in `hash-embedder:v1` says nothing about a row embedded in `hash-embedder:v2`.
- A `NOVELTY` whose baseline generation is a **different embedding space** than the rows it scores is the **invariant-9 error** — the same refusal §13 gives a cross-space ranking, because a cross-space *distance* is exactly as meaningless as a cross-space *score*. The message is written to teach, and names both spaces:

  ```
  this NOVELTY compares rows in hash-embedder:v2 against a baseline built in
  hash-embedder:v1 — a distance between two embedding spaces is not a distance
  (a cosine of 0.8 in one is not a cosine of 0.8 in the other). PrismDB will not
  compute it. Either name a baseline in hash-embedder:v2, or rebuild the baseline
  in this generation (`prism baseline build`).
  ```

- A baseline whose source generation was **redacted or retired out of retention** cannot be rebuilt, and the query does not silently score against a stale one: it is **DEGRADED, not silent** ([D-038](DECISIONS.md)) — the result carries the degraded state and the reason, exactly as `baselines_refresh` records it. A gate test drives the retention-expired path and asserts the query reports degraded rather than returning a confident wrong number.
