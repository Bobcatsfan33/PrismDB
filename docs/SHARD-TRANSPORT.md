# Coordinator-to-shard transport

PrismDB's node boundary is an mTLS shard RPC service and remote coordinator.
The coordinator routes tenant-scoped reads and writes to the owning shard and
carries the three operations used by a cross-tenant two-round query:

1. candidate discovery against an explicitly pinned snapshot;
2. exact reranking of the coordinator-selected rows; and
3. materialization of the final survivors.

It also exposes health, exact catalog snapshot discovery/validation, a
complete read against a pinned snapshot, and bounded single-tenant ingest.
Protocol v3 binds every
  coordinator-supplied snapshot to the shard's immutable catalog bytes before it
  is used, and every rerank/materialize part handle must belong to that pinned
  snapshot. Ingest exists only when the server is constructed with an
object-store-backed replicated WAL and has acquired the shard ownership epoch.
The protocol does not expose arbitrary catalog publication, ownership
acquisition, recovery controls, or WAL record IDs.

## Security and resource contract

- Mutual TLS is mandatory. The server trusts only certificates issued by the
  configured coordinator CA; the client validates both the shard CA and the
  configured DNS name. There is no plaintext or optional-client-auth
  constructor.
- Use separate, least-privilege cluster CAs for coordinator and shard
  identities. Do not reuse a public web PKI key or the object-store credentials.
- Private-key files must be regular, non-symlink files with mode `0640` or
  stricter on Unix. Group-read supports one isolated Kubernetes `fsGroup`;
  group write/execute and every permission for other users are refused.
  Certificate, CA, and private-key files are capped at 4 MiB and rejected from
  metadata before allocation.
- Every request names its protocol version, unique request ID, and intended
  shard. A shard refuses a request addressed to another shard.
- Coordinator topology is a versioned, deny-unknown-fields JSON file capped at
  1 MiB and must be a regular non-symlink. Shard IDs must be the contiguous
  range `0..N` and the coordinator caps a topology at 256 shards.
- Startup health checks every endpoint concurrently and refuses mixed immutable
  store configuration: format, dimensions, quantizer, seed, partition routing,
  and promoted columns must agree.
- Frames are four-byte big-endian length-prefixed JSON and are rejected before
  allocation above 16 MiB. Rerank/materialize selections are capped at 10,000
  rows and ingest batches at 1,000 events. Socket deadlines are mandatory and
  bounded to 10 ms–60 s.
- The production listener caps concurrent TLS handshakes and requests at 64.
  Excess connections are closed rather than spawning unbounded work.
- One connection carries one request and one response. Reads are deterministic
  against pinned state. Ingest retries rely on the existing per-tenant
  idempotency-key contract and return duplicate suppression explicitly.

## Run a shard endpoint

Provision a server certificate whose SAN matches the DNS name coordinators use,
a private key, and the CA that issues coordinator client certificates:

```text
prism shard-serve \
  --path /var/lib/prism/shard-0 \
  --listen 0.0.0.0:7443 \
  --shard-id 0 \
  --cert /run/prism-tls/shard-chain.pem \
  --key /run/prism-tls/shard-key.pem \
  --client-ca /run/prism-tls/coordinator-ca.pem \
  --timeout-ms 5000 \
  --write-enabled true
```

The command opens the normal production model and object-store configuration,
recovers the replicated admission log, emits a JSON startup record, and then
serves. Write mode requires `PRISM_S3_ENDPOINT`; the local object store is
refused because it cannot survive node loss. Omitting `--write-enabled true`
preserves the read-only server. Port `0` is refused outside the deterministic
test harness.

Rotate certificates with an overlap window: distribute the new CA bundle,
rotate leaf identities, prove every peer is using the new chain, then remove the
old CA. Revocation, issuance, and rotation evidence belong in the customer's PKI
system of record.

## Run the remote read coordinator

Create a topology containing only authenticated endpoint identity:

```json
{
  "version": 1,
  "shards": [
    {
      "shard_id": 0,
      "address": "prism-shard-0.internal:7443",
      "server_name": "prism-shard-0.internal"
    },
    {
      "shard_id": 1,
      "address": "prism-shard-1.internal:7443",
      "server_name": "prism-shard-1.internal"
    }
  ]
}
```

Then query through the coordinator identity:

```text
prism coordinator-search \
  --topology /etc/prism/topology.json \
  --cert /run/prism-tls/coordinator-chain.pem \
  --key /run/prism-tls/coordinator-key.pem \
  --shard-ca /run/prism-tls/shard-ca.pem \
  --query "payment service timeout" \
  --tenant tenant-a
```

Omit `--tenant` for the global two-round merge. The coordinator pins one
catalog snapshot per reachable shard, validates it, merges the global candidate
set under one fetch budget, reranks on owning shards, and materializes only
survivors. The default response fails with the shard named on any partition.
`--best-effort` is explicit per query and labels every dropped shard; if a shard
disappears during final materialization, the coordinator removes all of that
shard's scores and recomputes top-k so it cannot return a plausible but short
answer. Partial semantic `GROUP BY` remains refused.

## Evidence and remaining wall

The permanent gate proves:

- a trusted coordinator completes health, catalog-bound snapshot operations,
  full tenant reads, all three cross-shard query fragments, and a
  remote-durable tenant ingest;
- a read-only endpoint refuses ingest by construction;
- an untrusted coordinator certificate is rejected;
- a valid certificate for the wrong shard DNS name is rejected;
- a half-open peer hits the configured read deadline; and
- an oversized frame is rejected from its four-byte prefix before allocation;
- a two-shard remote query is byte-identical to the in-process coordinator;
- mixed store configurations, edited snapshot bytes, and part handles outside
  the pinned snapshot are refused; and
- real endpoint loss is fail-named by default, labelled only on explicit
  best-effort, refuses partial grouping, and recomputes top-k when loss occurs
  during materialization.

The authenticated multi-node read/write path is now wired. Remaining HA evidence
is sustained 1→4 node scaling on independent hosts and timer-driven asynchronous
hedging under latency/jitter, not an in-process or deterministic partition seam.
Already-published hot-part restoration remains behind the backup/hydration
increment. The public authenticated API is documented separately.
