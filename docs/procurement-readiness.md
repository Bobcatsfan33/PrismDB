# Enterprise procurement and deployment decision

[`enterprise-readiness.json`](enterprise-readiness.json) is the authoritative, expiring index of
PrismDB's product evidence and open gates. CI checks that every evidence path exists, that partial
controls name their gaps, and that a product with open blocking gates cannot be represented as
production-approved.

The current decision is **not approved**, and PrismDB as a whole is **not yet a software release
candidate**. The signed model-service image, mTLS remote coordinator, and authenticated `prismd`
API distribution are strong component artifacts. The public API now has exact-certificate tenant
policy, bounded requests, metrics, a hardened Helm chart, and a signed-image release path.
The admission log is remote-durable, replacement-node recovery is gated, and the authenticated
public ingest path injects tenant identity above a replicated-only shard RPC. The shard node itself
now has a signed, supported distribution: a digest-pinned non-root image, a StatefulSet chart with
per-shard identity and default-deny networking, startup gates that refuse a non-member topology, a
shared trust bundle, or a non-durable write target, and an admission policy requiring an approved
digest under the exact approved workflow identity. These components do not turn the database core
into a complete supported product: migration and version policy, production key custody and
lifecycle (per-tenant envelope encryption is implemented and gated, but every gate ran against the
software keystore, which proves the code path and not the custody — blocking gate `EXT-KMS`),
backup/hydration and RPO/RTO, load-derived SLOs, independent-host scaling/fault evidence,
independent penetration testing, support, and organizational assurance remain required.

## Evaluation baseline

The index is organized for [NIST SSDF 1.1](https://csrc.nist.gov/pubs/sp/800/218/final),
[SLSA 1.2](https://slsa.dev/spec/v1.2/),
[OWASP ASVS 5.0.0](https://owasp.org/www-project-application-security-verification-standard/),
[CSA CCM/CAIQ 4.1](https://cloudsecurityalliance.org/artifacts/cloud-controls-matrix-v4-1),
[CSA AI-CAIQ 1.0.2](https://cloudsecurityalliance.org/artifacts/ai-consensus-assessments-initiative-questionnaire-ai-caiq),
and [NIST AI RMF 1.0](https://www.nist.gov/publications/artificial-intelligence-risk-management-framework-ai-rmf-10)
review. AI RMF 1.0 is under revision, so its pinned version must be reassessed when NIST publishes
a final replacement.

SOC 2 or ISO 27001 reports, Shared Assessments SIG responses, legal and data-processing terms,
subprocessor and residency disclosures, insurance, financial viability, accessibility, support,
and reference checks are vendor or organization evidence. Passing repository CI is not a
substitute.

Run:

```bash
python3 scripts/verify_enterprise_readiness.py
```
