# Enterprise procurement and deployment decision

[`enterprise-readiness.json`](enterprise-readiness.json) is the authoritative, expiring index of
PrismDB's product evidence and open gates. CI checks that every evidence path exists, that partial
controls name their gaps, and that a product with open blocking gates cannot be represented as
production-approved.

The current decision is **not approved**, and PrismDB as a whole is **not yet a software release
candidate**. The signed model-service image, mTLS remote coordinator, and authenticated `prismd`
read-service distribution are strong component artifacts. The read API now has exact-certificate
tenant policy, bounded requests, metrics, a hardened Helm chart, and a signed-image release path.
They do not turn the database core into a complete supported product: a public write plane and shard
distribution, encryption and key lifecycle, backup and RPO/RTO, load-derived SLOs, independent-host
scaling/fault evidence, independent penetration testing, support, and organizational assurance
remain required.

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
