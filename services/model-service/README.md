# PrismDB model service

This is the production identity gateway between PrismDB and a colocated
[Text Embeddings Inference](https://github.com/huggingface/text-embeddings-inference)
(TEI) process. It is dependency-free Python, owns no model runtime, listens
only on a Unix socket, and refuses readiness until:

1. every model in the PrismDB registry has exactly one deployment;
2. every explicitly enumerated weights, tokenizer, and preprocessing byte
   matches the registry digest;
3. every backend URL is loopback-only;
4. TEI's health endpoint and a real dimension-checked warmup succeed.

The gateway normalizes finite, non-zero backend vectors and returns one result
per input under the exact identity PrismDB requested. It never logs input text.
A TEI failure becomes per-item errors; PrismDB then dead-letters the ingest
items or refuses the query without publishing partial semantic state.

## Prepare an immutable model

Clone or export the approved model into a read-only directory. Do not allow the
runtime to download from the Hub. Add `prism-preprocessing.json` using the
committed example, then enumerate every file that defines weights and
tokenization:

```bash
PYTHONPATH=services/model-service \
python -m prism_model_service digest \
  --root /models/approved \
  --weights model.safetensors \
  --tokenizer tokenizer.json config.json modules.json 1_Pooling/config.json \
  --preprocessing prism-preprocessing.json
```

Copy the three output hashes and derived `model_version` into
`model-registry.json`, then copy the same identity and file lists into
`model-service.json`. The digest algorithm frames each normalized relative path
and file length before streaming its bytes. Symlinks, traversal, duplicates,
missing files, and unenumerated implicit directory walks are refused.

## Run

Start one TEI process per registered model, each on a distinct loopback port
and with its model directory mounted read-only. Pin the TEI image by digest,
disable Hub/network access at runtime, and configure its request/payload bounds
to be no larger than PrismDB's. Then start the gateway:

```bash
PYTHONPATH=services/model-service \
python -m prism_model_service serve --config /etc/prism/model-service.json
```

The PrismDB process uses the same registry and the socket:

```bash
export PRISM_MODEL_REGISTRY=/etc/prism/model-registry.json
export PRISM_MODEL_SOCKET=/run/prism/model.sock
export PRISM_MODEL_TIMEOUT_MS=5000
```

An exec readiness/liveness probe performs a real peer-authorized round trip
without running inference:

```bash
PYTHONPATH=services/model-service \
python -m prism_model_service health --socket /run/prism/model.sock
```

The gateway is Linux-only in production because every connection is authorized
using `SO_PEERCRED`. Set `allowed_peer_uids` to the numeric UIDs of the PrismDB
process and the gateway health probe. The socket mode may not grant any access
to `other`; mount `/run/prism` from an in-memory, size-bounded volume shared
only by the colocated containers.

## Container

The gateway image has no package dependencies and runs as UID/GID 65532. The
Dockerfile default is itself pinned to an approved multi-architecture base
digest. Release builds pass the same digest explicitly and record both base and
resulting image digests in the deployment bill of materials:

```bash
docker build \
  --build-arg PYTHON_IMAGE=python:3.12.13-slim-bookworm@sha256:<approved-digest> \
  -t registry.example/prism-model-service:<release> \
  services/model-service
```

[`docs/RELEASE-ASSURANCE.md`](../../docs/RELEASE-ASSURANCE.md) defines the
blocking vulnerability gate, SBOM, keyless signature, build provenance,
promotion tag, and independent digest-verification procedure. Production
deployments consume the verified digest, never the example tag.

TEI is a separate, pinned container because it owns the GPU/CUDA crash
lifecycle. Its model path, pooling, prompt, truncation, batch-token limits,
client batch limit, and no-egress policy are deployment-controlled inputs to
the preprocessing and release evidence. The gateway does not turn a mutable
TEI tag or mutable model mount into an immutable deployment.
