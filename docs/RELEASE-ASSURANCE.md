# Model-service release assurance

PrismDB model-service releases are promoted by immutable image digest, never
by a mutable tag. The release workflow builds only from a digest-pinned base,
blocks fixable High or Critical vulnerabilities, emits an SPDX SBOM, publishes
the image to GHCR, signs it with Sigstore keyless signing, attaches the SBOM as
a signed attestation, and publishes GitHub build provenance.

## Pull-request and main-branch gate

`.github/workflows/model-service-release.yml` builds the same Dockerfile and
base digest used by the release job. It proves:

- the image records the approved base digest;
- the configured identity is exactly UID/GID `65532:65532`;
- the command runs with no network, no Linux capabilities, a read-only root
  filesystem, `no-new-privileges`, and only a small `noexec` temporary mount;
- Syft can inventory the complete image into SPDX JSON;
- Trivy finds no fixable High or Critical OS or library vulnerability.

The workflow retains the SBOM and machine-readable vulnerability report for
every run. An exception is not a line added to `.trivyignore`: a real waiver
requires a reviewed record naming the CVE, affected artifact, exploitability
analysis, owner, compensating control, and an expiry no more than 30 days away.
There is deliberately no waiver ledger until one is genuinely needed.

## Publish

After all branch protections and reviews pass, create a version tag whose
version is unique to this deployable:

```bash
git tag -s model-service-v0.1.0 <reviewed-commit>
git push origin model-service-v0.1.0
```

A repository ruleset must restrict `model-service-v*` tag creation to release
maintainers. The tag starts the publish job, which builds `linux/amd64` and
`linux/arm64`, pushes both under one OCI index digest, and records:

- BuildKit `mode=max` provenance in the OCI result;
- a Sigstore keyless signature bound to this repository, workflow, and tag;
- a signed SPDX SBOM attestation bound to the image digest;
- a GitHub artifact attestation for the same digest;
- a compact release receipt naming source revision, base digest, image digest,
  and release tag.

The evidence artifact retained by Actions is convenient, but it is not the
trust root. The signature, SBOM attestation, provenance, and image are all
addressed by the registry digest.

## Independent verification

Set `DIGEST` from the approved release receipt, not from a mutable tag:

```bash
export IMAGE=ghcr.io/bobcatsfan33/prism-model-service
export DIGEST=sha256:<approved-index-digest>
export TAG=model-service-v0.1.0

cosign verify \
  --certificate-identity \
  "https://github.com/Bobcatsfan33/PrismDB/.github/workflows/model-service-release.yml@refs/tags/${TAG}" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  "${IMAGE}@${DIGEST}"

cosign verify-attestation \
  --certificate-identity \
  "https://github.com/Bobcatsfan33/PrismDB/.github/workflows/model-service-release.yml@refs/tags/${TAG}" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  --type spdxjson \
  "${IMAGE}@${DIGEST}"

gh attestation verify "oci://${IMAGE}@${DIGEST}" \
  --repo Bobcatsfan33/PrismDB
```

Admission policy must reproduce those identity and issuer checks and allow
only an explicitly approved digest. A valid signature proves who built which
bytes; it does not authorize every correctly signed version for production.

## Base and action updates

The base digest and every third-party action are immutable inputs. Dependabot
may propose action updates. A base update is a reviewed change to both the
Dockerfile and workflow `PYTHON_IMAGE`, followed by the full image policy,
SBOM, vulnerability, protocol, and failure gates. Never change only the human
readable Python tag while retaining an unrelated digest.

## PrismDB read-service distribution

The same assurance contract applies independently to `prismd`. The
`.github/workflows/prismd-release.yml` workflow builds
`deploy/prismd/Dockerfile` from a digest-pinned Rust 1.75 builder into a
digest-pinned distroless Debian 12 `nonroot` runtime, verifies UID/GID 65532,
and runs `prismd version` with a read-only filesystem, no network, no
capabilities, and `no-new-privileges`.

The workflow also lints and renders `deploy/helm/prismdb`, proves that an
unpinned image and single replica fail schema validation, generates SPDX,
retains a machine-readable vulnerability result, and blocks fixable
High/Critical findings. A protected `prismd-v*` tag publishes amd64/arm64,
keylessly signs the index digest, attests its SPDX SBOM, publishes GitHub build
provenance, verifies the workflow/tag identity, and retains a release receipt.

Independent verification uses the same commands above with:

```bash
export IMAGE=ghcr.io/bobcatsfan33/prismd
export TAG=prismd-v0.1.0
```

The expected certificate identity changes to
`.github/workflows/prismd-release.yml@refs/tags/${TAG}`. Deployment admission
must allow the approved receipt digest, not the `prismd-v*` tag. This is a
supported API-coordinator distribution; it is not evidence for the shard-node
database distribution, which has its own workflow and its own receipt below.

## PrismDB shard-node distribution

`.github/workflows/prism-shard-release.yml` applies the same contract to the
database node itself. It builds `deploy/prism-shard/Dockerfile` — the exact
`prism shard-serve` binary path — from the same digest-pinned Rust 1.75 builder
into the same digest-pinned distroless Debian 12 `nonroot` runtime, and proves:

- the image records both approved base digests and is configured as
  UID/GID `65532:65532`;
- the identity the kernel *observed* is 65532:65532, read back from the owner of
  the bytes a store initialisation actually wrote to a mounted volume — not the
  identity the image config claims;
- the root filesystem genuinely refuses a write: the same command that succeeds
  against the data volume fails against the image filesystem;
- the command runs with no network, no Linux capabilities, `no-new-privileges`,
  and only a small `noexec` temporary mount;
- `deploy/helm/prism-shard` lints, renders, and passes strict `kubeconform`,
  producing a `StatefulSet` with a read-only root, non-root identity, a
  NetworkPolicy, a PDB, a headless Service, an explicit PVC retention policy,
  topology spread, and `replicas = len(topology.shards)`;
- no credential material appears anywhere in rendered output;
- a missing digest, a mutable tag in either `image.tag` or `image.repository`,
  one Secret serving both trust roles, an empty topology, and a termination grace
  period below the drain budget each fail to render;
- Syft can inventory the complete image into SPDX JSON; and
- Trivy finds no fixable High or Critical OS or library vulnerability.

A protected `prism-shard-v*` tag publishes amd64/arm64 under one OCI index
digest, keylessly signs it, attests its SPDX SBOM, publishes GitHub build
provenance, verifies the exact workflow-and-tag identity, and retains a release
receipt. Independent verification uses the commands above with:

```bash
export IMAGE=ghcr.io/bobcatsfan33/prism-shard
export TAG=prism-shard-v0.1.0
```

and the certificate identity
`.github/workflows/prism-shard-release.yml@refs/tags/${TAG}`.

### Signed is not authorized

The shard receipt carries `certificate_identity` and
`certificate_oidc_issuer` alongside the image digest, and
[`scripts/check_release_admission.py`](../scripts/check_release_admission.py) is
the deployment-side gate over them:

```bash
python3 scripts/check_release_admission.py \
  --receipt prism-shard.release.json \
  --approvals approved-releases.json
```

Admission requires **both** halves — the certificate identity must equal the
approved workflow at the approved tag by exact string comparison, and the image
digest must appear in an explicit approved-digest list. A policy that admitted
"anything this workflow signed" would authorize every future release
automatically, including one cut from a branch nobody reviewed; the workflow
would sign it just as happily. The publish job proves the refusal direction on
every release by re-running the check against an approval list that does not name
the digest it just built, and `--self-test` proves an unapproved digest, a
different workflow, and a different tag are each refused.

Release receipts contain identities and digests only. A receipt that carried a
credential would turn a 90-day build artifact into a 90-day secret; object-store
credentials reach a shard through the secret provider at pod start and are never
rendered by Helm or recorded by a release.

This is a supported shard-node distribution. It is not evidence for migration and
version policy, backup and hydration of already-published parts, envelope
encryption and KMS lifecycle, or customer-scale RPO/RTO — those remain open.
