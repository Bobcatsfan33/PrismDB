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
