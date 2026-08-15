# Versioning, compatibility, and upgrades

**Status:** supported PrismDB product contract. This policy covers the `prism`,
`prismd`, and `prism-shard` artifacts released from this repository. It does not
turn an unpromoted build into a production release and it does not waive the
external gates in [`enterprise-readiness.json`](enterprise-readiness.json).

## Version surfaces

PrismDB uses SemVer for product artifacts. Before `1.0`, a minor release may
change an experimental API; a patch release may not. Every release notes all
changes to the four independently versioned compatibility surfaces:

| Surface | Current | Compatibility promise |
|---|---:|---|
| Product artifacts | `0.0.1` | exact versions only until the first promoted release |
| Public HTTPS API | `v1` | additive changes only within `v1`; removals require `v2` |
| Shard RPC | `4` | coordinator and shard must match exactly; mixed versions fail before serving |
| Store layout | writes `2`, reads `1,2` | old layouts remain readable; new layouts are never guessed |
| Part format | writes `4`, reads `1,2,3,4` | committed fixtures remain readable; merge rewrites legacy parts immutably |

The public API version is a compatibility contract, not the package version.
An implementation may receive patches without changing `/v1`. Shard RPC is
deliberately stricter: rolling replacement uses compatibility preflight and
does not allow a mixed protocol fleet to accept traffic.

## Supported upgrade procedure

1. Verify the target release's signature, provenance, SBOM, digest, and release
   admission policy as described in [`RELEASE-ASSURANCE.md`](RELEASE-ASSURANCE.md).
2. Run `prism backup`, retain the resulting complete backup receipts and the
   object-store versions for the entire rollback window, then execute `prism
   verify` and a replacement-node `prism hydrate` drill against those bytes.
3. Compare the source and target release surfaces. The target must read the
   source store/part versions, and every shard and coordinator in the target
   topology must report the same shard RPC version before writes are enabled.
4. Replace shard nodes one at a time. A replacement hydrates and recovers its
   remote admission tail before readiness. Do not mix writable protocol
   versions.
5. Replace `prismd`, run authenticated readiness and one tenant-scoped read and
   write canary, then observe error rate, recovery lag, and saturation for the
   deployment's declared bake interval.
6. Only after the bake interval may an operator run merge to migrate legacy
   parts. Merge writes new immutable parts and atomically changes the catalog;
   it never edits the source bytes.

Skipping a released version is unsupported until its release notes explicitly
name and test that path. Store and part compatibility does not imply shard RPC
compatibility.

## Rollback

Before legacy-part migration, roll back by restoring the previous application
digest. After merge has published current-format parts, the previous build may
only be restored if it declares those part versions readable. Otherwise restore
the pre-upgrade backup into replacement nodes and fence the failed deployment's
ownership epoch. Never copy a live data directory backward and never rewrite a
part in place.

Database generation rollback (`prism rollback --to`) changes a catalog snapshot
inside one compatible binary release. It is not a substitute for application
rollback.

## Release and deprecation rules

- A format or protocol constant may change only with an updated compatibility
  manifest, executable fixtures or protocol tests, release notes, and this table.
- A readable on-disk version is not removed before a major product release and
  a documented offline migration exists.
- Public API fields may be added compatibly. A field is deprecated for at least
  one promoted minor release before removal in a new API major.
- Security fixes may shorten a compatibility window only when the release notes
  identify the vulnerability, the affected versions, and the required operator
  action.
- Backups are supported only while all referenced store, part, model, and key
  versions remain authorized and readable. Key retirement must respect the
  backup retention window.

## Enforced evidence

[`testing/compat/contract.json`](../testing/compat/contract.json) is the
machine-readable inventory. [`verify_compatibility_contract.py`](../scripts/verify_compatibility_contract.py)
fails CI if constants drift, frozen positive fixtures change, an evidence path
is missing, or this policy ceases to be indexed. The Rust integration suite
opens, verifies, queries, and immutably migrates the fixtures; the CI
compatibility job also exercises the released binary against them.

