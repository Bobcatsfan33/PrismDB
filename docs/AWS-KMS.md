# AWS KMS deployment and custody gate

PrismDB has a production `aws-kms` `KeyProvider`. It calls KMS `Encrypt` and
`Decrypt` for 32-byte data-encryption keys; event bodies and part contents never
leave the database process. The transport requires certificate-validating TLS
1.2+, SigV4, bounded socket deadlines, a 1 MiB response limit, and the fixed
encryption context `prism-purpose=dek-wrap-v1`.

This implementation makes the live custody exercise possible. It is **not**
itself evidence that any deployment has correct key custody. `EXT-KMS` remains
open until a retained receipt from a real account passes the complete procedure
below.

## Required configuration

- `PRISM_KMS_KEY_ID`: the active key's full immutable ARN. Alias names and bare
  key ids are refused.
- `PRISM_KMS_REGION`: the key region. It must match the ARN.
- `PRISM_KMS_DECRYPT_KEY_IDS`: optional comma-separated previous key ARNs kept
  for unwrap during rotation and the backup recovery window.
- Credentials use either `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and an
  optional `AWS_SESSION_TOKEN`, or a refreshable AWS shared-credentials file at
  `PRISM_KMS_CREDENTIALS_FILE` with `PRISM_KMS_PROFILE` (default `default`). The
  file is read on every KMS call so an external credential provider can rotate
  short-lived STS credentials atomically.

The shard Helm chart mounts KMS credentials from a Secret separate from object
storage credentials and adds only the configured private KMS endpoint CIDR to
egress. Prefer a KMS VPC endpoint. The chart never accepts credential bytes as
Helm values.

The KMS principal needs only:

```json
{
  "Effect": "Allow",
  "Action": ["kms:Encrypt", "kms:Decrypt", "kms:DescribeKey"],
  "Resource": ["<active-and-authorized-previous-key-arns>"],
  "Condition": {
    "StringEquals": {
      "kms:EncryptionContext:prism-purpose": "dek-wrap-v1"
    }
  }
}
```

Do not grant key creation, alias mutation, scheduling deletion, or unrestricted
`kms:*` to a shard. Key administration and data-plane use must be separate IAM
roles.

## Preflight and rotation

`prism key probe` round-trips a fresh ephemeral DEK and emits only the backend,
immutable key id, and ciphertext length. It never emits a plaintext key.

Rotation is deliberately split between safe database operations and the
external key ceremony:

1. **Expand:** add the old and new immutable ARNs to the KMS policy and to
   `PRISM_KMS_DECRYPT_KEY_IDS`.
2. **Activate:** set `PRISM_KMS_KEY_ID` to the new ARN and roll shards. Run
   `prism key probe` and `prism key status --path <store>`.
3. **Rewrap:** run `prism key rewrap --path <store>` on each fenced shard. It
   changes envelopes only, is idempotent, and reports its backend and counts.
4. **Retire check:** run `prism key retire-check --path <store> --key-id <old>`.
   PrismDB refuses while a live part or WAL envelope still needs the old key.
5. **Retire externally:** only after the recovery window and backup/key-policy
   check may the security role remove or disable the old KMS version.

The active key and every authorized previous key must be multi-region or have a
documented recovery-region equivalent consistent with the deployment's RTO. A
region-wide KMS outage blocks recovery by design.

## `EXT-KMS` acceptance exercise

Run in a non-production account with the production topology and retention
settings. Retain command output, CloudTrail event ids, key-policy revision,
release digest, and operator approval without retaining credentials or DEKs.

The receipt must prove all of the following against real KMS responses:

1. `key probe`, encrypted ingest, multi-part query, and backup/hydration succeed
   and receipts name `aws-kms`.
2. Expand → activate → rewrap → retire-check works across two real immutable key
   ARNs; part column bytes remain unchanged; restore works while the previous key
   is retired-but-authorized.
3. Real policy/network/service conditions produce unreachable, denied,
   throttled, and disabled/revoked failures. Each fails closed without a partial
   catalog or hydration install, and retry succeeds after restoration.
4. The complete D-094 replacement-node drill loses zero acknowledged events and
   records RPO/RTO under the approved topology.
5. Security Engineering records key administration, usage principals,
   multi-region posture, backup/key retention coupling, and CloudTrail alerting.

Copy [`live-kms-receipt.template.json`](../testing/evidence/live-kms-receipt.template.json),
replace every placeholder, attach the referenced evidence bundle, and run:

```bash
python3 scripts/verify_live_kms_receipt.py path/to/live-kms-receipt.json
```

The template intentionally fails verification. It is a gate definition, not a
passing receipt.

