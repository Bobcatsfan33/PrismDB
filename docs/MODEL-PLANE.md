# Production model-plane contract

PrismDB can run embedding inference in a separately supervised process over a
local Unix socket. The database process never loads model weights or a GPU
runtime. A CUDA fault, model-process crash, or model reload therefore cannot
write into the storage engine's address space.

The first S13 increment closed model identity, batch transport,
output-validation, crash, and reload safety. The second adds a runnable,
dependency-free identity gateway in front of a colocated Text Embeddings
Inference (TEI) process. The gateway independently hashes mounted artifacts,
admits only loopback backends and authorized Unix peers, performs a real
startup warmup, and normalizes/validates every backend vector. The third adds
exact per-tenant model/purpose grants, pre-ACK local usage budgets, and a
durable no-text ledger below every inference door. Versioned redaction,
fleet-wide quota/chargeback, query caching, drift/OOD calibration, and the
long-running API deployment that owns sidecar scaling remain open S13/S14
work. The gateway image now has a blocking vulnerability gate, SBOM, keyless
signature, and build provenance; see
[`RELEASE-ASSURANCE.md`](RELEASE-ASSURANCE.md).

## Immutable identity

A production model is the tuple of:

- model weights SHA-256;
- tokenizer SHA-256;
- preprocessing pipeline SHA-256.

`ModelArtifacts::revision()` hashes an unambiguous canonical form of that
tuple. A registry entry is accepted only when `model_version` equals this full
revision. Mutable aliases such as `latest` are refused.

Every production generation persists the three source digests, folds them into
its content address, and pins the resulting model version in every part. When
an existing generation is read, its artifact tuple must exactly match the
operator registry. The inference response must echo the same identity. A
service that restarts on different weights, tokenizer, preprocessing, or model
name produces a named error; its vectors are never used.

The deterministic `hash-embedder` remains available only when no production
model plane is configured. It is explicitly unable to serve a generation that
carries registered artifact provenance.

## Configuration

Both variables are required together:

```bash
export PRISM_MODEL_REGISTRY=/etc/prism/model-registry.json
export PRISM_MODEL_SOCKET=/run/prism/model.sock
export PRISM_MODEL_TIMEOUT_MS=5000
export PRISM_MODEL_POLICY=/etc/prism/model-policy.json
export PRISM_MODEL_AUDIT_LOG=/var/log/prism/model-usage.jsonl
```

The registry is bounded to 1 MiB and 128 models. Digests must be lowercase,
64-character SHA-256 values; dimensions must be in `1..=65536`; duplicate
identities and a missing default are refused. Start from
[`testing/model-registry.example.json`](../testing/model-registry.example.json)
and replace every example digest with the digest of the exact approved
artifact.

Startup performs a real warmup inference under the default identity before a
store is created or opened for a command. Partial configuration, an unavailable
socket, a dimension mismatch, an invalid registry, or a wrong response
identity fails startup. There is no fallback to the hash model after production
configuration is requested.

The socket file must be created with an owner/group and mode that permit only
the PrismDB process identity and the model-service identity. The deployment
manager owns those filesystem permissions and process supervision.

## Deployable identity gateway

[`services/model-service`](../services/model-service) implements the server
side of this protocol. It deliberately does not import or load model code.
Instead, it fronts one colocated TEI process per immutable model over an
explicit loopback URL. TEI owns CUDA, model batching, metrics, and its own
crash lifecycle; the gateway owns PrismDB identity and trust.

Before binding the Unix socket, the gateway:

- validates the same bounded registry as the Rust client;
- requires exactly one deployment for every registered model, including
  historical generations;
- content-addresses explicitly enumerated weights, tokenizer, and
  preprocessing files (no directory guessing, symlinks, traversal, or
  duplicate paths);
- requires the preprocessing manifest to name the exact supported TEI,
  truncation, normalization, input-bound, and prompt contract;
- permits only an explicit loopback HTTP backend;
- checks backend health and performs a real dimension/finiteness/norm warmup.

Each accepted Unix connection is authorized by Linux `SO_PEERCRED`, bounded by
a fixed worker count, and held to the client deadline and byte limits. Input
text is never logged. The exec health probe traverses the same peer-authorized
socket but does not spend GPU work.

The gateway container has no Python package dependencies and runs as
UID/GID 65532. Its release workflow supplies the Python base image by digest,
blocks fixable High/Critical findings, emits and attests an SPDX SBOM, signs the
published digest keylessly, and publishes build provenance. A deployment must
independently verify that evidence, pin the TEI image by digest, mount the model
and configuration read-only, use a memory-backed size-limited socket volume,
remove all Linux capabilities, enable a read-only root filesystem, and deny
runtime egress.
The exact operational recipe remains coupled to the S14 long-running PrismDB
API workload; pretending a one-shot CLI is an autoscaled server would create a
deployment artifact with no product behind it.

## Wire contract

One connection carries one newline-delimited JSON request and response. A
batch is bounded to 256 texts and 4 MiB of raw text; the encoded request is
bounded to 32 MiB. A response is bounded to 64 MiB. Socket reads and writes use
the configured deadline.

Request:

```json
{
  "protocol_version": 1,
  "model_id": "approved-model",
  "model_version": "<artifact revision>",
  "artifacts": {
    "model_sha256": "<sha256>",
    "tokenizer_sha256": "<sha256>",
    "preprocessing_sha256": "<sha256>"
  },
  "texts": ["first text", "second text"]
}
```

Response:

```json
{
  "protocol_version": 1,
  "model_id": "approved-model",
  "model_version": "<artifact revision>",
  "artifacts": {
    "model_sha256": "<sha256>",
    "tokenizer_sha256": "<sha256>",
    "preprocessing_sha256": "<sha256>"
  },
  "outputs": [
    {"status": "ok", "vector": [0.1, 0.2]},
    {"status": "error", "error": "input rejected"}
  ]
}
```

The number of outputs must equal the number of inputs. Every successful vector
must have the registered dimension, contain only finite values, and arrive
with L2 norm in `[0.999, 1.001]`. PrismDB normalizes only the residual
floating-point drift after that strict gate. Invalid items are dead-lettered;
a global transport failure dead-letters the batch. If no item survives, no part
or catalog snapshot is published.

## Permanent gates

- registry aliases and non-canonical digests are refused;
- batch inference is one transport call;
- wrong identity, wrong dimension, non-finite, and wrong-norm output fail
  closed;
- the Unix-socket protocol round-trips under its bounds;
- mounted artifact bytes are verified before the gateway becomes ready;
- non-loopback backends, symlinked artifacts, and unauthorized peer UIDs are
  refused;
- TEI health plus a real warmup gate readiness, and malformed backend vectors
  preserve output cardinality while failing every affected item closed;
- partial CLI configuration fails before store creation, while complete
configuration performs a real identity-checked startup warmup;
- a production model configuration without the paired tenant policy and usage
  ledger fails before store creation;
- tenant/model/version/purpose denial happens before an ingest WAL ACK and has
  the stable `model_policy_denied` reason;
- direct and SQL queries share the same exact-purpose authorization;
- model usage audit records identity, purpose, bytes, and outcome but never
  input text, and an unwritable ledger fails inference closed;
- an honest model creates a generation carrying exact artifact hashes;
- a wrong-model reload cannot answer against that generation;
- a model crash during ingest dead-letters the complete batch and leaves the
  prior snapshot and part set byte-for-byte unchanged.
