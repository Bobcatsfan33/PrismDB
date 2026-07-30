# Product incident and vulnerability response

This runbook covers vulnerabilities and incidents affecting PrismDB source, storage formats,
catalogs, model identities, shard transport, object storage, and the model service. It supplements
the private reporting channel in [`SECURITY.md`](../SECURITY.md); it does not claim a staffed
on-call or customer-notification program that does not yet exist.

## Classification

- **Critical:** cross-tenant access, unauthenticated remote execution, signing-key compromise,
  undetected durable corruption, or a model-identity bypass that can produce plausible wrong
  answers.
- **High:** exploitable denial of service, confidential-data exposure within a tenant, rollback or
  admission bypass, or loss of recovery integrity.
- **Medium/low:** bounded defects without demonstrated confidentiality, integrity, or availability
  impact.

Preserve the report, affected versions and digests, timestamps, logs, and reproduction material.
Do not place customer data, secrets, private keys, or exploit details in a public issue.

## Contain and determine scope

1. Freeze affected releases and preserve the corresponding source, workflow, SBOM, provenance,
   image, and test evidence.
2. Identify affected PrismDB versions, CPU/ISA paths, format versions, generations, model digests,
   catalogs, protocol versions, and deployment topologies.
3. Revoke affected image or certificate admission, quarantine suspicious parts, and run
   `prism fsck` on retained copies before attempting repair.
4. If model identity is implicated, stop promotion and migration, retain the exact model,
   tokenizer, preprocessing, and codebook digests, and treat answers produced under an
   unverified identity as suspect.
5. Preserve immutable evidence before catalog rollback, garbage collection, or credential
   rotation changes the observable state.

## Remediate and recover

The fix must be DCO-signed and receive the normal tests plus focused regression, fuzz, fault,
determinism, dependency audit, and release-assurance coverage appropriate to the defect. Build from
a clean revision and record exact source, artifact, image, SBOM, and provenance digests.

Restore only from verified parts and catalogs. Validate tenant isolation, model identity,
checksums, query determinism, and recovery behavior in the target topology before returning
traffic. Re-run admission and release verification against the remediated artifacts.

## Close and learn

Retain a timeline, root cause, affected-version matrix, containment and recovery evidence, and
corrective actions with owners. Add a permanent regression test and update the threat model,
contracts, runbooks, and readiness index when the incident changes a control assertion.

Named 24x7 response staff, an exercised production incident drill, customer and regulator
notification procedures, and independent penetration-test validation are deployment gates; they
remain explicitly open in [`enterprise-readiness.json`](enterprise-readiness.json).
