# Security Policy

## Current status

PrismDB is an executable reference core under active development, not a supported production
service. The current release does not provide a public production network server and authentication
boundary, cross-node write transport, or per-tenant envelope encryption. The read-only
coordinator↔shard path does require mutual TLS. See `README.md` and `docs/PROGRESS.md` before placing
any sensitive data in a store.

## Reporting a vulnerability

Do not file suspected vulnerabilities as public issues. Use GitHub private vulnerability reporting
and include the affected revision, reproduction steps, impact, and any proposed mitigation.

The maintainers will acknowledge reports within three business days and provide an initial severity
assessment within seven business days. Critical issues affecting a published artifact are targeted
for remediation within seven calendar days and high-severity issues within thirty days.

## Supported versions

Until a 1.0 release, only the latest revision of `main` is eligible for security fixes. That policy
is suitable for evaluation builds only; a supported-version and migration policy is a prerequisite
for production readiness.
