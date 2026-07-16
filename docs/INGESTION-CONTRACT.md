# The Ingestion Contract

**Status:** written *before* S2 was built, on the architect's instruction — the S2 gate risk is duplicate/replay semantics, not the schema. This is the one page that has to be right. Everything in `crates/prism-engine/src/admission.rs`, `wal.rs`, `idempotency.rs` and `source.rs` implements exactly what is written here, and the tests assert this document rather than the code.

Where this contract and the code disagree, **the contract is right and the code is a bug.**

---

## 1. The one-sentence version

An event that is acknowledged is an event that will be queryable, exactly once, with its semantic columns — and an event that cannot be acknowledged is an event you can *see*, in the dead-letter log, with a named reason.

There is no third outcome. In particular there is no "accepted, stored, and silently missing its embedding", and no "acknowledged, then lost in a crash".

---

## 2. What an idempotency key covers

**The key is `(tenant_id, idempotency_key)`,** where `idempotency_key` defaults to `event_id` and may be set explicitly by the producer.

It is scoped **per tenant**. Two tenants may use the same key for different events; that is not a collision, and one tenant must never be able to suppress another tenant's event by guessing an id.

**The key identifies the event, not the payload.** Alongside it we store a `content_hash` — the SHA-256 of the event's admitted content (body, event_time, event_name, cost, error, attributes). The pair is what makes the three outcomes below distinguishable.

### The three outcomes, and the rule

| The producer sends… | We do… | Because |
|---|---|---|
| a **new** key | admit it | it is a new event |
| a key we have seen, with **the same content hash** | **acknowledge it, do not store it again**, count it as `duplicates_suppressed` | this is a **replay** — a retry after an ack that got lost, a source re-delivering after a crash. The producer is doing exactly the right thing and must not be punished for it. |
| a key we have seen, with a **different content hash** | **dead-letter it**, reason `idempotency_conflict` | this is a **conflict**, not a replay. Something has changed underneath a key that was supposed to identify one event. Silently overwriting stored data on the strength of a reused id is how you lose an audit trail. We refuse, loudly, and keep both — the stored one, and the rejected one in the dead-letter log where a human can compare them. |

**Rejected: last-write-wins at admission.** It is the seductive option and it is wrong here: it makes a replayed-but-mutated event silently rewrite history, and it makes the system's behaviour depend on message *arrival order*, which no producer controls.

### The honest limit

The idempotency index is **bounded** — it retains keys for a window (`idempotency_window`, default 7 days of event time, and a hard entry cap). Beyond that window a replayed key is no longer recognised and will be admitted as a new event.

Such an event is then a **duplicate row**, and duplicates are reconciled at merge by the policy that has been documented since S0 ([D-012](DECISIONS.md)): **last write wins by `event_time`; ties go to the later part.**

So there are two distinct mechanisms and they answer two distinct questions:

- **Idempotency (admission time)** — "have I already accepted this exact event?" Bounded, exact, and it is what makes replay safe.
- **Merge reconciliation** — "two rows carry the same `event_id`; which one is real?" Unbounded in time, and it is the backstop.

A system that only had the second would double-count every retry until the next merge. A system that only had the first would silently diverge once its window rolled over. We say both, and we say where the seam is.

---

## 3. The ack point, and invariant 7

> **Invariant 7:** source offsets / idempotency records advance **with-or-after** publication, never before.

Two things are being promised and they are not the same promise.

### 3a. The producer's ack

An ack means **durable**, not **visible**. A batch is acknowledged once it has been appended to the **durable admission log (WAL)** and `fsync`ed. At that instant, the events are guaranteed to become queryable — even if the process dies immediately afterwards, because recovery will replay the WAL.

**S11 extends what "durable" covers, without moving the ack** ([D-068](DECISIONS.md)). Once the cold tier lives on remote object storage, a part is durable only when its bytes are **remote-verified** ([storage contract §2](STORAGE-CONTRACT.md)) — so the WAL's coverage stretches from "embed → part write → catalog commit" to "embed → part write → **upload → verify → CAS publication**". The ack still fires at the local WAL fsync (not at a remote round trip), and it still means *the WAL will carry these events all the way to remote-durable-and-visible*: recovery re-embeds, re-writes the part, **re-uploads and re-verifies it**, and re-commits, landing exactly once. A part written locally but not yet uploaded-verified-and-referenced is **neither queryable nor acked-durable on its own** — the WAL is what makes its events durable, and there is no local-only-and-trusted third state. The ack-after-log choice is deliberate; the synchronous alternative (ack waits for the upload) is written down and rejected in [D-068](DECISIONS.md).

### 3b. The source's offset

A source offset is **only ever advanced after the catalog commit that makes those events visible.** Never on ack. Never on WAL append.

This is deliberate, and the asymmetry is the whole point:

```
poll source                            offset = 100
  → admission checks
  → WAL append + fsync            ←──  ACK to producer here
  → embed
  → write immutable part
  → catalog commit                ←──  events are now VISIBLE
  → advance source offset              offset = 200
```

A crash **anywhere** before the catalog commit leaves the source offset at 100. The source will re-deliver events 100–200. They will be recognised by the idempotency index (or by the WAL recovery, which runs first) and land **exactly once**.

A crash **after** the catalog commit but **before** the offset advance leaves the source offset at 100 as well. The source re-delivers, and the idempotency index recognises every one of them as a replay and suppresses them. **Offsets may lag reality; they must never lead it.** Lagging costs a redundant poll. Leading loses data permanently.

### The crash that matters most

**Between embedding and the part write.** The event has been acked (it is in the WAL), it has consumed GPU time, and it exists nowhere durable except the WAL. The test `an_event_acked_then_crashed_before_the_part_write_reappears_exactly_once` drives precisely this, at the `part.after_write_before_fsync` kill point, and asserts the event is queryable afterwards, **once**, **with its embedding** — not stored blind, not lost, not doubled.

---

## 4. `event_time` vs `observed_time`

Two timestamps, two jobs. Confusing them is how a telemetry system quietly stops being able to answer questions about the past.

| | meaning | who sets it | what it is used for |
|---|---|---|---|
| `event_time` | when the thing **happened** | the producer | **partitioning, zone maps, retention, all time predicates** |
| `observed_time` | when we **received** it | the admission boundary | lag measurement, debugging, the skew check below |

**Partitions key on `event_time`.** Always. A query for "yesterday afternoon" means yesterday afternoon, not "whatever arrived yesterday afternoon". Agent telemetry is late by nature — a trace is often flushed minutes after the span it describes — so keying on arrival would smear every trace across partitions and make time pruning worthless.

### Accepted skew

An event is admitted only if its `event_time` is within the accepted skew window of `observed_time`:

- **`max_lateness`** (default **7 days**): how far in the *past* an `event_time` may be. Beyond it, the partition it belongs to may already have been merged, tiered, or expired by retention, and admitting it would resurrect a closed partition.
- **`max_skew_ahead`** (default **1 hour**): how far into the *future* an `event_time` may be. A clock-skewed producer emitting timestamps months ahead would poison zone maps and retention forever — a single bad event with `event_time = 2099` makes its partition immortal.

An event outside either bound is **dead-lettered** with reason `event_time_too_late` or `event_time_in_future`, carrying both timestamps and the bound it broke. It is **never** clamped to the boundary: silently rewriting a producer's timestamp is falsifying their data.

---

## 5. Attributes are bounded before they exist

> *"`attributes` is where formats go to die."*

Every limit below is enforced at admission, and every violation is **dead-lettered with a named reason** — never silently truncated. A truncated attribute map is a lie that no one will ever catch.

| Limit | Default | Dead-letter reason |
|---|---|---|
| keys per event | 64 | `too_many_attribute_keys` |
| key length | 128 B | `attribute_key_too_long` |
| value length | 4 KiB | `attribute_value_too_long` |
| total attribute bytes per event | 16 KiB | `attributes_too_large` |
| **distinct attribute keys per partition** | **512** | `attribute_key_cardinality_exceeded` |

**The last one is the one that matters.** The first four bound the size of one event; only the fifth bounds the *shape of the data*. A tenant emitting `user_id_<uuid>` as an attribute *key* will produce a key dictionary the size of their traffic, and every part will carry it. That is how a columnar format dies.

So attribute **keys** are a **bounded dictionary per partition**. When a partition's dictionary is full, an event introducing a new key is **refused at admission** — not absorbed, not spilled, not silently dropped from the map. The tenant is told, in the dead-letter log, that they are emitting unbounded key cardinality, which is a bug in *their* instrumentation and one they can only fix if we tell them about it.

Attribute **values** are unbounded in cardinality (a `session_id` value is fine, and normal); only keys are dictionary-bounded.

Promotion of hot attributes to typed columns is **[issue #2](https://github.com/Bobcatsfan33/PrismDB/issues/2), targeted at S4** — deliberately not built in S2, because promotion only pays once the block-skipping machinery it depends on exists.

---

## 6. Quotas, and starvation

> *"One tenant cannot exceed quota or starve others."*

Per-tenant, per-window limits, enforced at admission, **before** any GPU time is spent:

| Quota | Default |
|---|---|
| events per second | 10,000 |
| bytes per second | 32 MiB |
| in-flight batch bytes | 64 MiB |

Over-quota events are **rejected with reason `quota_exceeded`**, carrying the tenant, the limit, and the observed rate. They are *not* queued — an unbounded queue is a quota that does not exist, it is just a slower way to run out of memory.

**Starvation** is the separate failure, and the more insidious one. A tenant that is *within* quota can still monopolise a batch simply by being loud. So admission is **round-robin across tenants within a batch**: each tenant contributes at most `batch_size / active_tenants` events per batch before the next tenant gets a turn. A tenant with one event per second is admitted at the same latency whether or not another tenant is pushing ten thousand.

The test is not "the big tenant was throttled". The test is **"the small tenant's latency did not change when the big tenant arrived"**, which is the thing the small tenant actually cares about.

---

## 7. The OTel GenAI mapping is versioned, like a generation

The GenAI semantic conventions **are still moving**. Field names have changed twice already (`gen_ai.completion` → `gen_ai.output.messages`, and the `gen_ai.usage.*` shape). A mapping that silently follows the latest convention would silently change what a stored column *means*, which is the exact failure that immutable generations exist to prevent.

So: the mapping pins a **semantic-convention version** (`SEMCONV_VERSION`), it is recorded in the store and in every part's provenance, and a payload written under one convention version is never reinterpreted under another. Changing the mapping is a **new mapping version**, and old data keeps its old meaning.

**What gets embedded is a product decision, and the contract says to make it deliberately.** For the default `traces` schema, the embedded `body` is the **prompt and completion content**, in that order, and nothing else — not the tool JSON, not the parameters, not the token counts. Those are scalars and attributes; they are things you *filter* by, not things you search *by meaning*. Embedding them dilutes the vector with syntax and makes "find traces that resemble this failure" return traces that share a temperature setting.

---

## 8. What S2 does not build

Named here so nobody mistakes silence for completeness:

- **No network listener.** No OTLP/gRPC server, no HTTP endpoint. The OTLP **mapping** is real and tested against real OTLP/JSON payloads, and events are ingested from a file or a stream. The server is `prismd`, and it is S14.
- **No Kafka client.** The `Source` abstraction has exactly Kafka's offset semantics — poll, publish, then commit the offset — and the file-backed source implements it, so invariant 7 is tested for real. Wiring a broker is a transport detail, not a semantics one.

Both are logged in [DECISIONS.md](DECISIONS.md).
