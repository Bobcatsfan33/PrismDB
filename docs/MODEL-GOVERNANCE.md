# Production model governance

PrismDB's production model plane denies inference unless the authenticated
tenant has an exact grant for the requested model id, full artifact-derived
model version, and purpose. The gate sits below ingest, direct search, SQL,
generation migration, and evidence evaluation; a caller cannot select a
different public door to bypass it.

## Required configuration

Production inference requires both:

```bash
export PRISM_MODEL_POLICY=/etc/prism/model-policy.json
export PRISM_MODEL_AUDIT_LOG=/var/log/prism/model-usage.jsonl
```

Start from
[`testing/model-policy.example.json`](../testing/model-policy.example.json).
The policy is bounded to 1 MiB and 10,000 tenants. It must be a regular,
non-symlink file with no group or other permission bits; deploy it with mode
`0400` or `0600`. Unknown JSON fields, duplicate tenants, duplicate grants or
purposes, mutable/non-digest versions, empty grants, zero limits, and any
default action other than `deny` are refused.

The audit parent directory must already exist. The audit file must be regular,
non-symlinked, and mode `0600`; an existing file accessible to group or other
users is refused. If the ledger cannot be written and synchronized, inference
fails closed.

`PRISM_ALLOW_UNGOVERNED_MODEL=true` is a conspicuous development-only escape
hatch. Without it, configuring a production registry/socket without governance
fails before creating or opening a store.

## Exact grants

Each grant binds:

- one tenant id;
- one exact `model_id`;
- one full 64-character artifact-derived `model_version`;
- one or more purposes: `ingest`, `query`, `migration`, or `evaluation`.

Purposes are intentionally independent. Permission to ingest telemetry does
not permit arbitrary semantic queries. Query permission does not permit a
generation migration to read and re-embed retained bodies. Evidence tooling
does not inherit customer query access.

An ingest denial uses the stable `model_policy_denied` dead-letter reason. It
is written before any durable admission-log ACK and never reaches model
execution or a part. Query and migration denials are named errors. Production
query execution without a tenant context is denied.

## Local safety budgets

`max_inputs_per_minute` and `max_input_bytes_per_minute` are fixed-window,
per-process safety bounds scoped to `(tenant, model id, model version)`. Ingest
reserves the budget before the durable ACK and does not consume it again during
WAL replay. Query, migration, and evaluation charge immediately before model
execution.

These bounds protect one process and one local GPU from accidental or hostile
overload. They are not a distributed commercial quota: replicas do not share
the in-memory window, and a restart resets it. Enterprise billing and
fleet-wide rate enforcement still require a durable external quota authority
at the S14 API boundary.

## Usage ledger

Every inference attempt and denial emits one JSON line containing:

- schema version and timestamp;
- tenant id;
- purpose;
- exact model id and version;
- UTF-8 input byte count;
- outcome (`ok`, `error`, or `denied`).

Input text, embeddings, policy details, model paths, socket paths, and backend
errors are never written to this ledger. A batch synchronizes the ledger before
returning usable vectors. The file is a local append-only operational source;
ship it with an approved log agent to immutable centralized storage before
using it for investigation or chargeback.

## Why redaction is not in this increment

Tenant-specific redaction changes the bytes presented to the model and
therefore changes the embedding space. A mutable rule outside the hashed
preprocessing artifact would let the same `model_version` mean different
vectors, and a WAL replay after a policy edit could produce a different vector
for the same acknowledged event. That violates the core no-space-mixing
invariant.

Redact-before-embedding therefore remains open until the gateway protocol
carries tenant context and the complete redaction profile is content-addressed
inside `preprocessing_sha256`. An upstream DLP system may redact before PrismDB
today, but PrismDB does not claim that external control as an in-repository
gate.
