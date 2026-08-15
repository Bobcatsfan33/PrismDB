# Security Policy

## Current status

PrismDB has a supported authenticated `prismd` HTTPS boundary, a mutually authenticated
coordinator↔shard read/write protocol, and per-tenant envelope encryption. Its signed service and
shard distributions, hardened Helm manifests, and version/upgrade contract are implemented.
PrismDB as a whole is nevertheless **not production-approved**: the authoritative open controls
and external deployment gates are in `docs/enterprise-readiness.json`. In particular, software
keystore tests do not establish production KMS custody, and independent penetration, topology,
operational, and organizational evidence remains required. See `README.md` and
`docs/procurement-readiness.md` before placing sensitive data in a store.

## Reporting a vulnerability

Do not file suspected vulnerabilities as public issues. Use GitHub private vulnerability reporting
and include the affected revision, reproduction steps, impact, and any proposed mitigation.

The maintainers will acknowledge reports within three business days and provide an initial severity
assessment within seven business days. Critical issues affecting a published artifact are targeted
for remediation within seven calendar days and high-severity issues within thirty days.

## Supported versions

Until the first promoted release, only the exact current product version is eligible for security
fixes. Promoted releases will publish their support window and affected versions in release notes.
The compatibility, upgrade, rollback, and deprecation contract is
`docs/VERSIONING-AND-UPGRADES.md`; CI binds its machine-readable inventory to the executable API,
RPC, store, and part-format constants.
