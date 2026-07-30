# Coordinator-to-shard transport

PrismDB's first real node boundary is a read-only shard RPC service. It carries
the three operations the two-round distributed query already uses:

1. candidate discovery against an explicitly pinned snapshot;
2. exact reranking of the coordinator-selected rows; and
3. materialization of the final survivors.

It also exposes health and snapshot discovery. It does **not** expose ingest,
catalog publication, ownership acquisition, or recovery. Those mutations remain
local until the admission log is remote-durable, because allowing another node
to take over without the acknowledged-but-unpublished records would weaken the
ack contract.

## Security and resource contract

- Mutual TLS is mandatory. The server trusts only certificates issued by the
  configured coordinator CA; the client validates both the shard CA and the
  configured DNS name. There is no plaintext or optional-client-auth
  constructor.
- Use separate, least-privilege cluster CAs for coordinator and shard
  identities. Do not reuse a public web PKI key or the object-store credentials.
- Private-key files must be regular, non-symlink files with mode `0600` or
  stricter on Unix. Certificate, CA, and private-key files are capped at 4 MiB
  and rejected from metadata before allocation.
- Every request names its protocol version, unique request ID, and intended
  shard. A shard refuses a request addressed to another shard.
- Frames are four-byte big-endian length-prefixed JSON and are rejected before
  allocation above 16 MiB. Rerank/materialize selections are capped at 10,000
  rows. Socket deadlines are mandatory and bounded to 10 ms–60 s.
- The production listener caps concurrent TLS handshakes and requests at 64.
  Excess connections are closed rather than spawning unbounded work.
- One connection carries one request and one response. All current operations
  are deterministic reads against pinned state and can be retried safely.

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
  --timeout-ms 5000
```

The command opens the normal production model and object-store configuration
before listening, emits a JSON startup record, and then serves. Port `0` is
refused outside the deterministic test harness.

Rotate certificates with an overlap window: distribute the new CA bundle,
rotate leaf identities, prove every peer is using the new chain, then remove the
old CA. Revocation, issuance, and rotation evidence belong in the customer's PKI
system of record.

## Evidence and remaining wall

The permanent gate proves:

- a trusted coordinator completes health and all three query fragments;
- an untrusted coordinator certificate is rejected;
- a valid certificate for the wrong shard DNS name is rejected;
- a half-open peer hits the configured read deadline; and
- an oversized frame is rejected from its four-byte prefix before allocation.

This increment establishes the authenticated shard service and client. The
existing `sharded::Cluster` still invokes local engines directly; wiring its
fan-out through `TlsShardClient`, then driving real network partitions,
asynchronous hedge timing, and 1→4 node scaling is the next query-HA increment.
Cross-node write failover remains a separate durability increment behind a
remote-durable admission log.
