# The Encryption Contract

**Status:** written in S14 before the implementation, the discipline every other contract was
written under ([D-095](DECISIONS.md)). Envelope encryption changes what a stored byte *means*, so
the rules go down before the code does.

The fact it rests on: **encryption is a property of stored bytes, never of an answer.** A query over
an encrypted store and the same query over the same data unencrypted return byte-identical results.
This is the fifth member of the [D-033](DECISIONS.md) answer-invariance family — after layout
([C-4](DECISIONS.md)), ISA ([C-5](DECISIONS.md)), seed/scan-order ([C-7](DECISIONS.md)), and cache
state ([storage §3](STORAGE-CONTRACT.md)). A cipher that changes an answer is the same class of bug
as a layout-dependent tie-break.

---

## 1. The key hierarchy, and what each level is for

- **KMS key (the wrapping key).** AWS KMS in production; a software keystore in staging implementing
  the *same interface and the same failure modes*. It never sees plaintext data — only DEKs.
- **DEK (the data encryption key).** Tenant-scoped. Generated locally, wrapped by the KMS key, and
  stored **only** in wrapped form. The plaintext DEK exists in a bounded in-memory cache and nowhere
  else.
- **Key id.** Every ciphertext header carries the **explicit** id of the KMS key and the DEK epoch
  that produced it. A key id is never inferred from context, never defaulted, never guessed. A part
  that does not say which key encrypted it is refused, not probed.

## 2. What a DEK is scoped to — and the co-tenant limit, stated plainly

A part is encrypted under one DEK. Which DEK depends on how the tenant is placed, and the two cases
give genuinely different guarantees:

- **Dedicated bucket** (`--dedicated tenantA,...`): the part holds exactly one tenant's rows, so it
  is encrypted under **that tenant's DEK**. This is full per-tenant cryptographic isolation: tenant
  A's key cannot decrypt tenant B's bytes, and that is provable with real ciphertexts.
- **Shared bucket**: a part may hold rows from several tenants. It is encrypted under the
  **bucket DEK**. This protects the data at rest against anyone without that key — but it does
  **not** cryptographically separate co-tenants inside the bucket, because they share the key that
  opens the part.

> **The limit is named, not papered over.** Cryptographic isolation *between co-tenants* requires
> the tenants be placed in dedicated buckets. Shared-bucket encryption is at-rest confidentiality,
> not co-tenant separation. Any deployment that needs the stronger claim must place those tenants
> dedicated; PrismDB will not pretend a shared key is a separation boundary. Tenant isolation as an
> *I/O property* (the S4 gate — a tenant's query never opens another tenant's part) is unchanged and
> unaffected either way.

## 3. Granularity: per block, because ranged reads are a contract

Encryption is applied **per framed block**, never per whole file.

A whole-file cipher would break three things that are already contracts: ranged fetches
([storage §6](STORAGE-CONTRACT.md)'s byte-budgeted rerank), per-block CRC-32 named-byte errors
([S1](PROGRESS.md)), and the cold-tier cache's block-level admission and verification
([storage §4](STORAGE-CONTRACT.md)). Per-block encryption keeps all three: a ranged read still
fetches whole blocks, still verifies them, and still names a truncation by its byte shortfall.

**Associated data binds each block to its place.** The AEAD's AAD covers the part id, the column
name, the block index, and the key id. A block cannot be moved to another position, another column,
another part, or replayed under a different key without the tag failing. Confidentiality alone would
leave block-swapping open; the AAD closes it.

## 4. The AEAD, and why the nonce is stored rather than derived

**XChaCha20-Poly1305** (RustCrypto `chacha20poly1305`), with an **algorithm id in the header** so the
choice is versioned and swappable exactly as the rerank encoding is ([D-003-resolved](DECISIONS.md)) —
a name or an assumption that hard-codes today's cipher becomes a lie the day it is not.

Each block carries a **random 192-bit nonce** in its encryption header. XChaCha20's extended nonce
makes random selection safe without any counter, coordination between writers, or birthday-bound
accounting — which matters precisely because publication is distributed and ownership can change
epochs mid-life ([D-076](DECISIONS.md)). The cost is 24 bytes per block: 0.15% at the 16 KiB default
block size, and the thing it buys is that there is no nonce-reuse invariant for a future sprint to
get wrong.

## 4a. Two verification paths, and they are not interchangeable

Encryption splits what used to be one idea into two, and conflating them is how a keyless path ends
up claiming a guarantee it cannot deliver:

| | **Stored-byte integrity** | **Logical identity** |
|---|---|---|
| What it answers | "are these the bytes that were written here?" | "is this the same data?" |
| Mechanism | per-block CRC-32; per-file SHA-256 in a backup receipt | the part's content address |
| Needs the key? | **No** | **Yes** — the address names the plaintext |
| Used by | restore verification, cache verification, GC, corruption reporting | naming a part, deduplication, merge identity |

**A content address names logical content, not stored bytes.** An encrypted part and its plaintext
twin share an address; so do two sealings of the same data under different DEKs or nonces, which are
byte-identical in *no* position. That is the correct semantics — an address is a fact about data —
but it means **"same content address" must never be read as "same file"**.

Two consequences are load-bearing and are enforced in code:

- **No keyless path may claim content-address verification.** Hydration verifies a restored part
  before it can unwrap anything, and what it verifies is *stored-byte integrity* — the receipt's
  SHA-256 over the ciphertext. It is not, and must not be described as, a check that the restored
  part *is the right data*; that check requires the key and happens when the AEAD tag is verified on
  read.
- **Size and address together are not an identity test for encrypted parts.** Sealing is
  length-deterministic, so two sealings of one logical part share both an address *and* a byte
  length. Any "already present, skip the upload" shortcut keyed on those would be sound for
  plaintext and wrong for ciphertext, and could leave a stale object no live DEK opens.
  `publish_part_cold` and `backup_part` therefore always upload an encrypted part.

## 5. Coverage — the whole D-094 path, not just the part files

Encryption follows the data everywhere it is durable:

| Surface | Treatment |
|---|---|
| Published part column files | per-block AEAD under the part's DEK |
| Part manifest metadata | sensitive fields encrypted (see §6) |
| Backup objects ([D-094](DECISIONS.md)) | the backed-up bytes are already ciphertext; the receipt's sensitive fields are encrypted (§6) |
| Hot-tier / cold-tier cache | caches ciphertext blocks; plaintext is never written to the cache directory |
| Remote WAL payloads | admitted events are encrypted before the record is durable |
| Restored artifacts | hydration installs ciphertext and decrypts on read — never stages plaintext |

**Plaintext never touches disk.** Not in the cache, not in a staging directory, not in a temporary
file. A path that would need to write plaintext to durable storage is a design error, not an
optimization to guard with a flag.

## 6. Metadata confidentiality — the DATA-01 gap

Today a part manifest and a backup receipt name their tenants in plaintext, so anyone with raw disk
or bucket access learns **which tenants exist and which share a bucket** without decrypting a single
row. That is the raw-disk metadata-confidentiality gap DATA-01 records.

The rule that resolves it: **plaintext metadata is limited to what the restore path must read before
it can unwrap anything.**

- **Stays plaintext** — the part id, the file list with lengths and SHA-256 (integrity is verified
  before decryption, so a corrupt object is named without a key), the key id, the algorithm id, and
  the **bucket ordinal**. A bucket ordinal identifies placement without naming a tenant.
- **Becomes ciphertext** — the tenant list and any tenant-scoped statistics that would disclose
  co-tenant existence or volume.

Hydration's routing check ([D-094](DECISIONS.md)'s wrong-tenant refusal) uses the plaintext **bucket
ordinal**, which is exactly what routing is a function of, so the check keeps working without the
plaintext tenant names it never actually needed.

> **Where this is done, and where it is not — measured, not assumed.** The **backup receipt** is
> closed: an encrypted store's `BACKUP.json` names no tenant, and the gate greps the receipt's bytes
> for every tenant the part holds. Two surfaces are **still plaintext and are not claimed otherwise**:
>
> - the **part manifest**'s tenant list and `TenantStats` (`manifest.bin`, which must stay readable
>   without a key because it carries the envelope — the sensitive *fields* within it are what still
>   need sealing);
> - the **catalog mirror** snapshot in the object store, whose `PartEntry::Located` carries
>   `tenants` so a part can be pruned without being opened.
>
> So DATA-01 today reads: raw bucket or disk access still discloses **which tenants exist and which
> share a bucket**, while disclosing **no row content** — that part is closed, and the encrypted
> D-094 drill asserts no event body appears in any object under `parts/`, `wal/`, `catalog/` or
> `generations/`. Closing the remaining two is metadata work, not row-confidentiality work, and it
> is listed unticked below rather than folded into a claim it has not earned.

## 6a. Tenant identity at rest is a keyed token ([D-096](DECISIONS.md))

§6 named two surfaces it did not close: the **part manifest**'s tenant list and `TenantStats`, and the
**catalog mirror**'s `PartEntry::Located.tenants`. Both are closed here, and the mechanism is fixed by
what the read path actually needs.

**Routing and pruning need tenant *equality*, not tenant *identity*.** The catalog asks "is this
part's tenant the one I am querying for?" and "do these rows belong to the same tenant?" — both
answered by comparing opaque handles. No read path recovers a name, so no stored field needs to
contain one.

**The token.** `HMAC-SHA256(store_tenant_key, tenant_name)`, truncated to 128 bits, where
`store_tenant_key` is **store-scoped and derived from the existing key hierarchy** (§1). No new
custody, no new ceremony, nothing extra to rotate.

> **An unkeyed hash is forbidden.** Tenant names are **low-entropy** — short, human-chosen, drawn
> from a small realistic space, and usually *already known* to whoever holds the disk. Their question
> is not "who exists in the world" but "**which of them are on this box, and which share a bucket**".
> A bare `SHA-256(tenant)` answers that by **dictionary lookup**: precompute every plausible name and
> the map inverts completely. An unkeyed digest is also **identical in every deployment**, so it
> correlates the same tenant across stores, backups and customers — a global join key wearing the
> costume of a protection. The keyed construction defeats both, and because the key is store-scoped,
> **the same tenant tokenizes differently in different stores** — asserted by a gate, not claimed.

**What stays plaintext, unchanged.** The **bucket ordinal** (§6): hydration's wrong-shard refusal
must work before anything is unwrapped, routing is a function of the bucket and never of the name, and
an ordinal identifies placement without naming a tenant.

**What the token is not.** It authorizes nothing. Tenant authorization stays exact-certificate policy
at the service boundary, above the engine.

**How a token renders to an operator.** `prism inspect` shows the **token**, labelled as one, with
a one-line note that it is a keyed handle and not a name. It does **not** silently print an opaque
hex string where a reader expects a tenant — unlabelled gibberish is how an operator concludes the
store is corrupt. And it cannot "resolve" tokens in the general case: HMAC is one-way, so the only
honest resolution is **forward** — a caller who already has candidate names may tokenize them and
match. A store configured with its key may therefore render `token(name)` pairs for names it was
given; with no key service, or for a name nobody supplied, the token stands alone. Inverting a token
is not a feature that was omitted; it is arithmetic that does not exist.

**The residual leak, named.** Token *equality* is still visible: an attacker without the key can see
that some tenant occupies N parts and shares a bucket with some other tenant. Sealing identity is not
sealing the existence of distinct tenants, and nothing here claims it is.

## 7. Key material never appears anywhere it can be read later

Plaintext DEKs live in a **bounded cache, zeroized on drop**, and nowhere else. Key material — DEKs
plaintext or wrapped, and any KMS response — must never appear in **logs, metrics, audit events,
manifests, backup receipts, error messages, `EXPLAIN` output, evidence receipts, or a release
receipt**. An error that names a key does so by **id**, never by value. This extends the P10 rule
that a receipt carrying a credential turns a build artifact into a secret.

## 8. Failure is named, and fails closed

Each of these is a distinct, named refusal — never a fallback to plaintext, never a partial answer:

| Condition | Behaviour |
|---|---|
| **Key service unreachable** | refuse by name; **no partially-decrypted state**; a retry after access returns succeeds cleanly |
| **Revoked key** | refuse by name; catalog state uncorrupted |
| **Missing key / unknown key id** | refuse by name; never probe other keys |
| **Wrong tenant** | refuse by name — a part whose bucket does not route here is a routing fault |
| **Wrong version** | an algorithm id or envelope version this build does not implement is refused, never guessed |
| **Corrupted ciphertext** | AEAD tag failure is a named corruption, distinct from a CRC failure and from a truncation |

**Fail-closed without corruption is the property**, not merely fail-closed. Decryption sits *inside*
the [D-094](DECISIONS.md) staging boundary — decrypt, verify, then rename — so an unwrap failure
mid-restore leaves staging directories and an untouched `CURRENT`, exactly as a corrupt-file failure
already does, and the retry-succeeds-cleanly half follows from the same property.

> **Operator responsibility, stated because the software cannot enforce it.** A **region-wide KMS
> outage is a recovery blocker by design**: an encrypted store cannot be restored without the key
> service, and PrismDB will refuse rather than invent a way around it. **AWS multi-region key
> replication is the operator-level mitigation** — an availability choice the deploying organisation
> makes. This is the same shape as [storage §10](STORAGE-CONTRACT.md)'s object-versioning duty: the
> recovery window a deployment can promise is the one its key and bucket policies actually support.

## 9. Rotation: expand → activate → rewrap → retire

Immutable parts stay immutable. **Rewrapping touches wrapped-DEK envelopes and never part bytes.**

1. **Expand** — a new KMS key version is available; both are accepted for unwrap.
2. **Activate** — new DEKs wrap under the new key; existing parts are untouched and still readable.
3. **Rewrap** — wrapped-DEK envelopes are re-encrypted under the new key. No part byte is rewritten,
   no nonce changes, no re-encryption of data occurs, and the operation is idempotent and resumable.
4. **Retire** — the old key stops being accepted. Refused while any live envelope still needs it —
   the same shape as [generation retire](PROGRESS.md), which refuses while a retained snapshot names
   the generation.

**Restore must work through the current key and through a retired-but-authorized previous key**, so
a backup taken before a rotation is not made unreadable by the rotation.

## 10. The format change is explicit, and old stores are untouched

Unlike [D-094](DECISIONS.md)'s `BACKUP.json` — a new object in a key space no reader parses — this
**is** a format change, and it engages the reserved mechanism deliberately:

- `FEATURE_ENCRYPTION` (bit 6) was reserved in S4 for exactly this and is **not** in
  `SUPPORTED_FEATURES` today, so current builds already refuse an encrypted part rather than
  misread it. This increment adds it to the supported mask.
- The envelope rides a **required TLV extension** (`EXT_REQUIRED` set), so a reader that does not
  understand it refuses the part instead of skipping essential metadata.
- **The feature flag is explicit, never inferred.** A store is encrypted because it was configured
  to be, not because a part happened to carry a bit.
- **An unencrypted store is byte-identical to before.** No feature bit, no extension, no new bytes,
  no behaviour change — the compatibility fixtures prove it.

## 11. What is claimed, and what is not

Everything gated here is exercised against the **software keystore** unless a live-KMS run is
recorded, and **every receipt names the backend that produced it** — the backend-conditional receipt
discipline from [storage §5](STORAGE-CONTRACT.md), now covering key custody. A staging run proves
the *code*; it does not prove the *custody*. The **key ceremony itself remains external** (Enterprise
PKI), and no gate in this repository claims otherwise.

---

## 12. Gate checklist — what every encryption gate owes, permanently

These are standing requirements on *any* future encryption gate, not a list that was satisfied once.

**Every encryption gate must include at least one query that touches two or more encrypted parts.**

Not a style preference — this is a defect that shipped. A **fresh DEK is minted per part**, so many
parts share one wrapping key id and one DEK epoch while holding entirely different keys. The
resident-key cache was keyed `{wrapping_key_id}#{dek_epoch}`, which does not identify a DEK, so the
first part's key was handed to every part opened after it. Any read spanning two encrypted parts
failed outright.

It survived a full sprint of gates because **every one of them filtered to a single tenant and so
opened a single part** — the cache was never asked a second question. It was found only when a
rotation gate happened to enumerate every part in the store, and it was found as a
`failed authenticated decryption` on an unrelated assertion. A single-part fixture cannot see this
class of bug at all, and neither can a gate that only *writes* several parts without reading across
them.

So a gate that exercises one part exercises the encryption of one part. Concretely, a gate owes:

- a read spanning **≥ 2 encrypted parts** in one process, through one resident-key cache;
- an assertion that the fixture **really did** produce more than one part, and that those parts
  really do carry **distinct wrapped DEKs** — otherwise the gate silently stops covering the case
  the day the corpus or the partition scheme changes shape.

`a_query_spanning_several_encrypted_parts_opens_each_under_its_own_dek` is the reference shape.

The cache key now includes `sha256(wrapped_dek)`, which does identify the DEK and is derivable
without unwrapping anything.

---

## Implementation checklist

- [x] `chacha20poly1305` pinned (`=`) and MSRV-1.75 clean; `zeroize` stays pinned at 1.8.1
- [x] `KeyProvider` trait: `wrap`, `unwrap`, `key_id`, with identical error taxonomy across backends
- [x] Software keystore backend with an **injectable fault surface** (unreachable, revoked, denied,
      throttled) so staging exercises the same branches KMS would take
- [x] Bounded DEK cache, zeroized on drop, never logged
- [x] `FEATURE_ENCRYPTION` added to `SUPPORTED_FEATURES`; `EXT_S14_ENCRYPTION` registered
- [x] Per-block AEAD with AAD = part id ‖ column ‖ block index ‖ **DEK label**; 192-bit random nonce
      stored. *Amended from "key id" during implementation:* the AAD's key component names the
      **DEK**, not the wrapping key. Binding it to the wrapping key made §9 rotation structurally
      impossible — every block failed its tag the instant its envelope was rewrapped. Key
      *wrapping* still binds the wrapping key id, because there the key in question really is the
      wrapping key.
- [x] **Backup-receipt** sensitive fields encrypted; bucket ordinal stays plaintext
- [ ] **Manifest** sensitive fields sealed (tenant list, `TenantStats`) — **designed** as the §6a
      keyed token ([D-096](DECISIONS.md)); not yet implemented
- [ ] **Catalog mirror** `PartEntry` tenant list sealed — same design, same state
- [x] Remote WAL payload encryption — **and the local admission log too**, which the coverage table
      did not name. The local WAL holds the same acked-but-unpublished rows on node-local disk, so
      §5's "plaintext never touches disk" would have been false with only the remote one sealed.
- [x] Rotation: expand / activate / rewrap / retire, with retire refused while envelopes need the
      key — counting **published parts and the admission log**, because an acked record holds a
      wrapped DEK too
- [x] Gates: cross-tenant decryption refused with real ciphertexts; key loss/revocation fails closed;
      restore through current **and** retired-but-authorized key; KMS-unreachable hydration
      (refuses by name, no partial state, clean retry); feature-flag rollback for never-encrypted
      stores; the full [D-094](DECISIONS.md) drill green **with encryption enabled**
- [x] CLI key service (`PRISM_STAGING_KEYSTORE_FILE`) — added because without it `prism hydrate`
      could never restore an encrypted store, so the drill could not stay process-isolated and no
      deployment could operate the feature at all
- [ ] Unsafe-posture inventory updated for whatever the implementation proves refusable
- [x] DATA-01 gap text records exactly what was exercised, and against which backend (§6)
- [ ] CRYPTO-01 gap text
- [ ] **Live-KMS run** — every gate above was exercised against the **software keystore**. It proves
      the code path; it does not prove the custody, and no receipt in this repository says otherwise.
