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
