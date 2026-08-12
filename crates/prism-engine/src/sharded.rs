//! The distributed cluster (S12) — a set of shards, each an independent [`Engine`], sharded by
//! **tenant bucket** ([D-071](../../../docs/DECISIONS.md)).
//!
//! A tenant bucket never straddles two shards ([S4](../../../docs/PRISM.md) isolation becomes the
//! placement boundary), so a tenant's data is whole on exactly one shard, and a tenant-scoped query
//! routes to that one shard. **Sharding is a layout** ([query §20](../../../docs/QUERY-CONTRACT.md)):
//! the same corpus on 1, 2, or 4 shards answers byte-identically, because which shard a tenant lives
//! on — and how many shards exist — is erased by routing (for a tenant-scoped query) and by the merge
//! (for a cross-tenant one, the filed next increment).
//!
//! **Increment scope.** This lands the cluster scaffold — tenant-bucket sharding, routing, and the
//! global **snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)) — and gates that
//! tenant-scoped queries are a layout. The cross-shard merge (the two-round global-candidate-set
//! search and the coordinated canonical-shard-order `GROUP BY`, [query §20](../../../docs/QUERY-CONTRACT.md))
//! is the next increment, built against the now-locked contract.

use crate::cluster::{ClusterRequest, SemanticClusterResult};
use crate::engine::Engine;
use crate::search::Scored;
use crate::storage::object::{LocalObjectStore, ObjectStore};
use prism_part::catalog::GcReport;
use prism_part::generation::Generation;
use prism_part::partition::{bucket_ordinal, PartitionScheme};
use prism_part::store::StoreConfig;
use prism_types::error::{PrismError, Result};
use prism_types::{Counters, Event, MissingShard, Query, SearchResult};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The read-only surface a coordinator needs from a shard. Both an in-process [`Engine`] and the
/// authenticated network client implement this exact contract, so local and multi-node queries run
/// one coordinator algorithm. Writes are deliberately absent: cross-node mutation cannot ship until
/// the admission log is remote-durable without weakening the ACK contract.
pub(crate) trait ReadShard: Sync {
    fn validate_snapshot(&self, snapshot: &prism_part::catalog::Snapshot) -> Result<()>;
    fn candidates(
        &self,
        snapshot: &prism_part::catalog::Snapshot,
        query: &Query,
    ) -> Result<crate::search::ShardCandidates>;
    fn rerank(
        &self,
        snapshot: &prism_part::catalog::Snapshot,
        query: &Query,
        selected: &[(String, usize)],
    ) -> Result<Vec<crate::search::ShardScored>>;
    fn materialize(
        &self,
        snapshot: &prism_part::catalog::Snapshot,
        selected: &[(String, usize)],
    ) -> Result<Vec<(Event, u32)>>;
}

impl ReadShard for Engine {
    fn validate_snapshot(&self, snapshot: &prism_part::catalog::Snapshot) -> Result<()> {
        for part_id in snapshot.part_ids() {
            if !self.store.part_dir(&part_id).exists() {
                return Err(PrismError::NotFound(format!(
                    "snapshot `{}` names part `{part_id}`, which has been reclaimed",
                    snapshot.snapshot_id
                )));
            }
        }
        Ok(())
    }

    fn candidates(
        &self,
        snapshot: &prism_part::catalog::Snapshot,
        query: &Query,
    ) -> Result<crate::search::ShardCandidates> {
        self.search_candidates(snapshot, query)
    }

    fn rerank(
        &self,
        snapshot: &prism_part::catalog::Snapshot,
        query: &Query,
        selected: &[(String, usize)],
    ) -> Result<Vec<crate::search::ShardScored>> {
        let live: BTreeSet<String> = snapshot.part_ids().into_iter().collect();
        if let Some((part_id, _)) = selected.iter().find(|(part_id, _)| !live.contains(part_id)) {
            return Err(PrismError::Invalid(format!(
                "selection names part `{part_id}`, which is not in pinned snapshot `{}`",
                snapshot.snapshot_id
            )));
        }
        self.search_rerank_selected(query, selected)
    }

    fn materialize(
        &self,
        snapshot: &prism_part::catalog::Snapshot,
        selected: &[(String, usize)],
    ) -> Result<Vec<(Event, u32)>> {
        let live: BTreeSet<String> = snapshot.part_ids().into_iter().collect();
        if let Some((part_id, _)) = selected.iter().find(|(part_id, _)| !live.contains(part_id)) {
            return Err(PrismError::Invalid(format!(
                "selection names part `{part_id}`, which is not in pinned snapshot `{}`",
                snapshot.snapshot_id
            )));
        }
        self.search_materialize(selected)
    }
}

/// **Test-only** ([query §21](../../../docs/QUERY-CONTRACT.md)): shard indices the coordinator treats
/// as unreachable, so the partial-failure path is exercised at the **coordinator boundary** without a
/// real network partition. What this proves is coordinator *semantics* — fail-named by default,
/// labelled-partial on opt-in — not transport-level partition behaviour, which stays a named wall.
static INJECTED_UNREACHABLE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Mark shard indices unreachable at the coordinator boundary (test seam). Empty clears it. Never a
/// production path.
pub fn inject_unreachable_shards(shards: &[usize]) {
    *INJECTED_UNREACHABLE.lock().expect("unreachable lock") = shards.to_vec();
}

fn shard_is_unreachable(si: usize) -> bool {
    INJECTED_UNREACHABLE
        .lock()
        .expect("unreachable lock")
        .contains(&si)
}

/// Resolve a shard that could not be reached ([query §21](../../../docs/QUERY-CONTRACT.md)): on a
/// **best-effort** query, record it in `missing` and carry on with the shards we can reach; otherwise
/// **fail, with the shard named** — never a silently short result. The default is fail-named because a
/// silently partial answer to a security or novelty question is the worst failure this product makes.
fn resolve_unreachable(
    si: usize,
    reason: &str,
    best_effort: bool,
    missing: &mut Vec<MissingShard>,
) -> Result<()> {
    if best_effort {
        if !missing.iter().any(|m| m.shard == si) {
            missing.push(MissingShard {
                shard: si,
                reason: reason.to_string(),
            });
        }
        Ok(())
    } else {
        Err(PrismError::NotFound(format!(
            "shard {si} unreachable: {reason}. This query touches it and did not opt in to a partial \
             answer, so it fails rather than return a silently short result (query §21). Pass \
             best_effort to accept a labelled partial answer."
        )))
    }
}

fn refuse_partial_group(query: &Query, missing: &[MissingShard]) -> Result<()> {
    if !missing.is_empty() && query.group_k.is_some() {
        let dropped: Vec<usize> = missing.iter().map(|entry| entry.shard).collect();
        return Err(PrismError::Invalid(format!(
            "a best-effort semantic GROUP BY dropped shard(s) {dropped:?}: a cluster distribution \
             over an incomplete shard set is not comparable to a complete run (cluster mass and \
             exemplars shift with the data present), so it is refused rather than returned as if \
             whole (query §21). Re-run without best_effort to fail by name, or narrow to a \
             reachable tenant."
        )));
    }
    Ok(())
}

/// **Test-only** ([D-079](../../../docs/DECISIONS.md)): shard indices to hedge — re-issue their
/// fragment — so the idempotence/dedup/blast-radius semantics are exercised. The synchronous
/// coordinator has no latency to trigger a hedge on its own; this stands in for the async transport's
/// `HEDGE_DELAY_MS` timer. Never a production path.
static INJECTED_HEDGE: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Test-only override of the in-flight blast-radius cap (`0` = use [`crate::hedge::MAX_INFLIGHT_FRAGMENTS`]).
static INJECTED_MAX_INFLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Mark shard indices as slow so the coordinator hedges their fragments (test seam). Empty clears.
pub fn inject_hedge_shards(shards: &[usize]) {
    *INJECTED_HEDGE.lock().expect("hedge lock") = shards.to_vec();
}

/// Override the in-flight blast-radius cap (test seam). `None` restores [`crate::hedge::MAX_INFLIGHT_FRAGMENTS`].
pub fn inject_max_inflight(cap: Option<usize>) {
    INJECTED_MAX_INFLIGHT.store(cap.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
}

fn shard_is_hedged(si: usize) -> bool {
    INJECTED_HEDGE.lock().expect("hedge lock").contains(&si)
}

fn effective_max_inflight() -> usize {
    let o = INJECTED_MAX_INFLIGHT.load(std::sync::atomic::Ordering::SeqCst);
    if o == 0 {
        crate::hedge::MAX_INFLIGHT_FRAGMENTS
    } else {
        o
    }
}

/// **Hedge a fragment** if its shard is marked slow and the blast-radius budget allows ([D-079](../../../docs/DECISIONS.md)).
/// `original` is the fragment the coordinator already has; this re-issues it up to [`crate::hedge::HEDGE_FANOUT`]
/// times, bounded so the total in flight never passes [`effective_max_inflight`] — a slow cluster must
/// not hedge itself into collapse. Because the re-issue runs against the **same pinned snapshot**, it is
/// **byte-identical**; a divergence is a named invariant violation (a fragment must be deterministic for
/// a hedge to be free). Dedup keeps the original. Accounts the original and each hedge in `inflight`, and
/// each hedge in `hedges`. A hedge that itself errors is dropped — the original stands.
fn maybe_hedge<T, F>(
    si: usize,
    original: &T,
    inflight: &mut usize,
    hedges: &mut usize,
    reissue: F,
) -> Result<()>
where
    T: PartialEq,
    F: Fn() -> Result<T>,
{
    *inflight += 1; // the original fragment
    if !shard_is_hedged(si) {
        return Ok(());
    }
    for _ in 0..crate::hedge::HEDGE_FANOUT {
        if *inflight >= effective_max_inflight() {
            break;
        }
        *inflight += 1;
        let dup = match reissue() {
            Ok(d) => d,
            Err(_) => continue, // the hedge failed; the original stands, uncounted.
        };
        if &dup != original {
            return Err(PrismError::Invariant(format!(
                "a hedged fragment for shard {si} diverged from its original against the pinned \
                 snapshot vector — a fragment must be deterministic for a hedge to be free of \
                 correctness risk ([D-079](docs/DECISIONS.md))"
            )));
        }
        *hedges += 1;
    }
    Ok(())
}

/// A cluster of shards. Each shard is a whole [`Engine`] over its own store; the cluster routes by
/// tenant bucket and never lets a tenant bucket straddle two shards. The **generation store** holds
/// the one cluster-global codebook, content-addressed, that every shard installs and serves
/// ([D-072](../../../docs/DECISIONS.md)) — shards never train their own.
pub struct Cluster {
    shards: Vec<Engine>,
    scheme: PartitionScheme,
    gen_store: Arc<dyn ObjectStore>,
}

/// The generation store key for a content-addressed codebook.
fn gen_key(id: &str) -> String {
    format!("generations/{id}")
}

/// A paginated cross-tenant query's cursor: the **pinned snapshot vector** (one id per shard) plus
/// the keyset position (score DESC, `event_id` ASC). Opaque and checksummed, exactly like the
/// single-node cursor ([query §19](../../../docs/QUERY-CONTRACT.md)).
#[derive(serde::Serialize, serde::Deserialize)]
struct ClusterCursor {
    snapshots: Vec<String>,
    last_score: f32,
    last_event_id: String,
}

impl ClusterCursor {
    fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        let mut out = format!("{:08x}", prism_types::hash::crc32(&json));
        for b in &json {
            out.push_str(&format!("{b:02x}"));
        }
        Ok(out)
    }

    fn decode(s: &str) -> Result<ClusterCursor> {
        let bad = || PrismError::Invalid("cursor is malformed".to_string());
        if s.len() < 8 || s.len() % 2 != 0 {
            return Err(bad());
        }
        let want = u32::from_str_radix(&s[..8], 16).map_err(|_| bad())?;
        let mut bytes = Vec::with_capacity((s.len() - 8) / 2);
        for pair in s.as_bytes()[8..].chunks_exact(2) {
            let h = std::str::from_utf8(pair).map_err(|_| bad())?;
            bytes.push(u8::from_str_radix(h, 16).map_err(|_| bad())?);
        }
        if prism_types::hash::crc32(&bytes) != want {
            return Err(PrismError::Invalid(
                "cursor failed its checksum; it has been truncated or edited".into(),
            ));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl Cluster {
    /// Create a cluster of `num_shards` shards under `root` (each shard a store `shard-<i>`), all
    /// sharing one partition scheme so a tenant hashes to the same bucket on every shard.
    /// Attach a key service to **every shard**, so a sealed cluster seals uniformly.
    ///
    /// Uniform by construction rather than by convention: a cluster with some shards sealed and some
    /// not would tokenize a tenant on one shard and name it on another, and the mixed-version match
    /// rule would paper over the difference instead of surfacing it. Each shard still mints its own
    /// store-scoped tenant key — shards are separate stores — which is why tokens are compared only
    /// within a shard and never across the wire.
    pub fn with_keys(mut self, keys: Arc<dyn crate::keys::KeyProvider>) -> Self {
        self.shards = self
            .shards
            .into_iter()
            .map(|s| s.with_keys(Arc::clone(&keys)))
            .collect();
        self
    }

    pub fn init(root: &Path, num_shards: usize, config: StoreConfig) -> Result<Cluster> {
        if num_shards == 0 {
            return Err(PrismError::Invalid(
                "a cluster needs at least one shard".into(),
            ));
        }
        let scheme = config.partitions.clone();
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            shards.push(Engine::init(
                &root.join(format!("shard-{i}")),
                config.clone(),
            )?);
        }
        let gen_store = Arc::new(LocalObjectStore::new(root.join("cluster-generations")));
        Ok(Cluster {
            shards,
            scheme,
            gen_store,
        })
    }

    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// A shard by index — for inspection and the routing gate (a tenant's answer through the cluster
    /// equals its answer read directly off the owner shard).
    pub fn shard(&self, i: usize) -> &Engine {
        &self.shards[i]
    }

    /// The shard that owns a tenant's bucket. A function of the bucket, not the tenant, so a whole
    /// bucket lives on one shard.
    pub fn shard_index(&self, tenant: &str) -> usize {
        let bucket = self.scheme.bucket_of(tenant);
        (bucket_ordinal(&self.scheme, &bucket) % self.shards.len() as u64) as usize
    }

    /// The global **snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)): each shard's
    /// live catalog seq. A distributed query pins this at planning; a tenant-scoped query needs only
    /// its owner shard's element, but the vector is the cluster's one consistent instant.
    pub fn snapshot_vector(&self) -> Result<Vec<String>> {
        self.shards
            .iter()
            .map(|e| Ok(e.snapshot()?.snapshot_id))
            .collect()
    }

    /// The generation every shard serves (they serve one, uniformly), or `None` before the first
    /// ingest has installed it.
    pub fn installed_generation(&self) -> Result<Option<String>> {
        Ok(self.shards[0].snapshot()?.active_generation)
    }

    /// Reclaim superseded snapshots and their parts on **every shard**, at the monotonic lease-clock
    /// instant ([`crate::clock`], D-075). A **distributed reader lease is the conjunction of per-shard
    /// leases**: a cross-shard cursor pins one snapshot per shard ([query §19](../../../docs/QUERY-CONTRACT.md)),
    /// and each pinned snapshot is protected on its own shard for [`prism_part::catalog::LEASE_TTL_MS`]
    /// plus its derived grace — so a reader within its lease finds every shard's parts, and a crashed
    /// reader's snapshots age out per shard and are reclaimed, its stale cursor then getting the named
    /// expired-snapshot error. Because the clock is monotonic, a wall-clock jump on this host reclaims
    /// nothing early and keeps nothing forever.
    pub fn gc(&self, retain_snapshots: usize, dry_run: bool) -> Result<Vec<GcReport>> {
        self.gc_at(retain_snapshots, crate::clock::lease_now_ms(), dry_run)
    }

    /// GC every shard at an explicit lease-clock `now_ms` — the injection door a lease or chaos test
    /// drives (advance monotonic time by hand while skewing the wall clock underneath). The same
    /// instant fans to every shard: on a single host the shards share one lease clock, and each
    /// shard's reclaim is still entirely local to its own catalog.
    pub fn gc_at(
        &self,
        retain_snapshots: usize,
        now_ms: i64,
        dry_run: bool,
    ) -> Result<Vec<GcReport>> {
        self.shards
            .iter()
            .map(|s| s.catalog().gc_at(retain_snapshots, now_ms, dry_run))
            .collect()
    }

    /// Publish a trained codebook to the cluster's generation store, content-addressed and
    /// idempotent — the store is a codebook's natural home ([D-071](../../../docs/DECISIONS.md)).
    fn publish_generation(&self, g: &Generation) -> Result<()> {
        g.verify_content_address()?;
        let key = gen_key(&g.generation_id);
        if self.gen_store.head(&key)?.is_none() {
            self.gen_store.put(&key, &serde_json::to_vec(g)?)?;
        }
        Ok(())
    }

    /// Install a published generation on **every** shard: fetch-by-hash, verify the bytes hash to the
    /// id asked for (the capability check — the store cannot hand a shard the wrong codebook), then
    /// activate. Returns only once every shard serves it: the **order invariant**
    /// ([D-071](../../../docs/DECISIONS.md)) — no shard writes a part pinned to a generation, or
    /// serves a query against it, before every assigned shard has installed and verified it.
    fn install_generation_everywhere(&self, id: &str, now_ms: i64) -> Result<()> {
        let bytes = self.gen_store.get(&gen_key(id))?;
        let g: Generation = serde_json::from_slice(&bytes)?;
        if g.generation_id != id {
            return Err(PrismError::Corrupt(format!(
                "the generation store returned `{}` for key `{id}` — not the codebook asked for",
                g.generation_id
            )));
        }
        for shard in &self.shards {
            shard.install_generation(&g, now_ms)?;
        }
        Ok(())
    }

    /// Ingest a batch, routing each event to the shard that owns its tenant bucket. **The first
    /// ingest trains the one cluster-global generation over a cluster-wide sample** (every event, not
    /// one shard's slice — a per-shard codebook is the [D-072](../../../docs/DECISIONS.md) mistake),
    /// publishes it, and installs it on every shard *before* any part is written. Thereafter every
    /// shard codes under the same codebook, so the same corpus lands byte-identically on 1, 2, or 4
    /// shards.
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<()> {
        if self.installed_generation()?.is_none() {
            // Train cluster-wide, seeded on the empty snapshot id so the codebook is identical at any
            // shard count. `train_generation` does not commit — the install path does.
            let seed_snapshot = prism_part::catalog::Snapshot::empty().snapshot_id;
            let (trained, _dead) =
                self.shards[0].train_generation(&seed_snapshot, events.clone())?;
            if let Some(t) = trained {
                self.publish_generation(&t.generation)?;
                self.install_generation_everywhere(&t.generation.generation_id, now_ms)?;
            }
            // If nothing embeds, no generation is installed and each shard's ingest finishes empty.
        }

        let mut by_shard: Vec<Vec<Event>> = vec![Vec::new(); self.shards.len()];
        for e in events {
            let s = self.shard_index(&e.tenant_id);
            by_shard[s].push(e);
        }
        for (i, batch) in by_shard.into_iter().enumerate() {
            if !batch.is_empty() {
                self.shards[i].ingest(batch, now_ms)?;
            }
        }
        Ok(())
    }

    /// A tenant-scoped search, routed to the owner shard. Which shard that is, and how many shards
    /// the cluster has, are invisible to the answer ([query §20](../../../docs/QUERY-CONTRACT.md)).
    /// A cross-tenant query (`tenant = None`) needs the global-candidate-set merge — the next
    /// increment — and is named, never silently answered from one shard.
    pub fn search(&self, q: &Query) -> Result<SearchResult> {
        match q.tenant.as_deref() {
            // A tenant-scoped query lives on one shard: route to it, unchanged.
            Some(t) => self.shards[self.shard_index(t)].search(q),
            // A cross-tenant query fans out: the two-round global-candidate-set merge (query §20).
            None => self.search_cross_shard(q),
        }
    }

    /// **The two-round cross-shard search** ([query §20](../../../docs/QUERY-CONTRACT.md),
    /// [D-073](../../../docs/DECISIONS.md)). Round 1: every shard returns its bounded candidates by PQ
    /// distance. The coordinator merges to the **global** candidate set (PQ distance, C-4 `event_id`
    /// tie) and bounds it once — to the rerank width and the **single global fetch budget**. Round 2:
    /// each owning shard exact-scores exactly its subset, so total exact fetches stay within that one
    /// budget. The coordinator then runs the **shared** `finalize` (the same code single-store search
    /// runs) over the merged scores, materializing the survivors back on their shards.
    fn search_cross_shard(&self, q: &Query) -> Result<SearchResult> {
        // Pin the snapshot vector AT PLANNING (query §19): one snapshot per shard, captured once and
        // read from for BOTH rounds — a publication landing mid-query cannot change the answer. A
        // cursor paginating this query carries exactly this vector.
        let vector = self.snapshot_vector_pinned()?;
        Self::coordinate_cross_shard(
            &self.shards,
            self.shards[0].store.config.dim,
            self.shards[0].store.config.seed,
            &vector,
            q,
            Vec::new(),
        )
    }

    /// The snapshots the coordinator pins for a query: one per shard, captured at planning.
    fn snapshot_vector_pinned(&self) -> Result<Vec<prism_part::catalog::Snapshot>> {
        self.shards
            .iter()
            .map(|s| s.snapshot())
            .collect::<Result<Vec<_>>>()
    }

    /// Pin the snapshot vector — one snapshot per shard, captured now. A paginated query captures
    /// this once and carries it in its cursor, so every page reads the same corpus ([query §19](../../../docs/QUERY-CONTRACT.md)).
    pub fn pin_vector(&self) -> Result<Vec<prism_part::catalog::Snapshot>> {
        self.snapshot_vector_pinned()
    }

    /// Answer a query against an explicitly **pinned snapshot vector** — the door a cursor resumes
    /// through. A tenant-scoped query reads its owner shard's pinned snapshot; a cross-tenant query
    /// runs the two-round merge against the whole vector. Nothing published after the vector was
    /// pinned is visible ([query §19](../../../docs/QUERY-CONTRACT.md)).
    pub fn search_at_vector(
        &self,
        vector: &[prism_part::catalog::Snapshot],
        q: &Query,
    ) -> Result<SearchResult> {
        match q.tenant.as_deref() {
            Some(t) => {
                let owner = self.shard_index(t);
                self.shards[owner].search_at(&vector[owner], q)
            }
            None => Self::coordinate_cross_shard(
                &self.shards,
                self.shards[0].store.config.dim,
                self.shards[0].store.config.seed,
                vector,
                q,
                Vec::new(),
            ),
        }
    }

    /// Load the pinned vector a cursor names, by snapshot id per shard. A snapshot id that no longer
    /// loads is the **expired** condition — the cursor's corpus has been reclaimed.
    fn load_vector(&self, ids: &[String]) -> Result<Vec<prism_part::catalog::Snapshot>> {
        if ids.len() != self.shards.len() {
            return Err(PrismError::Invalid(
                "this cursor is for a cluster of a different shard count".into(),
            ));
        }
        ids.iter()
            .enumerate()
            .map(|(si, id)| {
                self.shards[si].catalog().load_snapshot(id).map_err(|_| {
                    PrismError::NotFound(format!(
                        "the cursor's pinned snapshot vector is expired: shard {si}'s snapshot `{id}` \
                         has been reclaimed. Re-run the query to pin the current vector."
                    ))
                })
            })
            .collect()
    }

    /// **One page of a paginated query, resumed against the pinned snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)).
    /// Page 1 pins the vector; the returned cursor carries it, so every later page reads the same
    /// corpus — a publication (or a merge) landing between pages is invisible. The order is the same
    /// total order every result uses (score DESC, C-4 `event_id` ASC), so the pages tile the answer
    /// with no duplicate and no gap; the keyset is written longhand for that reason. Returns the page
    /// and the next cursor (`None` on the last page).
    pub fn search_page(
        &self,
        q: &Query,
        cursor: Option<&str>,
    ) -> Result<(SearchResult, Option<String>)> {
        let (vector, cur) = match cursor {
            None => (self.pin_vector()?, None),
            Some(tok) => {
                let body = ClusterCursor::decode(tok)?;
                (self.load_vector(&body.snapshots)?, Some(body))
            }
        };

        // Run against the pinned vector, materializing the whole ordered result set (up to the rerank
        // width) so pagination has something to page over — the survivors are the result set.
        let mut full = q.clone();
        full.k = q.rerank.max(q.k);
        let result = self.search_at_vector(&vector, &full)?;
        let ordered = &result.hits;

        // Keyset skip: "strictly after (last_score DESC, last_event_id ASC)". Longhand, because a
        // tuple compare would treat an equal-score smaller-id row as after and rewind pagination.
        let start = match &cur {
            None => 0,
            Some(c) => ordered
                .iter()
                .position(|h| match c.last_score.total_cmp(&h.score) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Equal => {
                        h.event.event_id.as_str() > c.last_event_id.as_str()
                    }
                    std::cmp::Ordering::Less => false,
                })
                .unwrap_or(ordered.len()),
        };

        let page: Vec<prism_types::Hit> = ordered.iter().skip(start).take(q.k).cloned().collect();
        let next = if start + page.len() < ordered.len() && !page.is_empty() {
            let last = page.last().unwrap();
            Some(
                ClusterCursor {
                    snapshots: vector.iter().map(|s| s.snapshot_id.clone()).collect(),
                    last_score: last.score,
                    last_event_id: last.event.event_id.clone(),
                }
                .encode()?,
            )
        } else {
            None
        };

        Ok((
            SearchResult {
                hits: page,
                ..result
            },
            next,
        ))
    }

    /// Execute the two-round merge against an **already-pinned** snapshot vector ([query §19](../../../docs/QUERY-CONTRACT.md)).
    /// Round 1 scans each shard's pinned snapshot; round 2 rescores the immutable parts those
    /// snapshots named. Nothing published after `vector` was captured can change the answer — which is
    /// what makes a mid-query (or mid-pagination) publication invisible.
    pub(crate) fn coordinate_cross_shard<S: ReadShard>(
        shards: &[S],
        dim: usize,
        seed: u64,
        vector: &[prism_part::catalog::Snapshot],
        q: &Query,
        mut missing: Vec<MissingShard>,
    ) -> Result<SearchResult> {
        if shards.is_empty() || vector.len() != shards.len() {
            return Err(PrismError::Invalid(
                "the coordinator needs one pinned snapshot per shard".into(),
            ));
        }
        let snaps = vector;
        let snapshot_id = snaps
            .iter()
            .map(|s| s.snapshot_id.as_str())
            .collect::<Vec<_>>()
            .join("+");
        let tombstones: BTreeSet<String> = snaps
            .iter()
            .flat_map(|s| s.tombstones.iter().cloned())
            .collect();

        // The pinned vector is only answerable while the parts it names still exist. The distributed
        // reader lease ([`gc`](Self::gc), D-075) protects a pinned snapshot on every shard for the
        // lease-plus-grace horizon, so a reader within its lease always finds these parts; a reader
        // past it (or crashed) has them reclaimed by GC, and the resumed query is **expired**, a named
        // condition ([query §2/§19](../../../docs/QUERY-CONTRACT.md)) — never a short answer.
        for (si, snap) in snaps.iter().enumerate() {
            if missing.iter().any(|entry| entry.shard == si) {
                continue;
            }
            if let Err(error) = shards[si].validate_snapshot(snap) {
                let reason = if matches!(&error, PrismError::NotFound(_)) {
                    format!(
                        "the pinned snapshot vector is expired: snapshot `{}` is no longer \
                         readable: {error}",
                        snap.snapshot_id
                    )
                } else {
                    format!(
                        "pinned snapshot `{}` validation failed: {error}",
                        snap.snapshot_id
                    )
                };
                resolve_unreachable(si, &reason, q.best_effort, &mut missing)?;
            }
        }

        // The shards a best-effort query could not reach (query §21). A non-best-effort query never
        // reaches the end of this list — `resolve_unreachable` fails it, by name, at the first miss.
        // Hedging bookkeeping (D-079): total fragments in flight (the blast radius) and hedges issued.
        let mut inflight = 0usize;
        let mut hedges = 0usize;
        let mut threshold_overfetch = 0usize;

        // --- round 1: candidates from every shard, merged to the global set ---
        // (dist, event_id, shard, part_id, row)
        //
        // **Fan out round 1 to every shard concurrently** — each shard's scan is independent, the
        // shared-nothing parallelism D-071 is judged on (the scaling verdict). `std::thread::scope`
        // borrows `self.shards`, `snaps`, and `q` without cloning; the per-shard results are processed
        // in shard order afterwards, so the merge is byte-identical to a sequential fan-out (the sort
        // erases arrival order regardless). A 1-shard cluster spawns one thread — negligible overhead.
        let initially_missing: BTreeSet<usize> = missing.iter().map(|entry| entry.shard).collect();
        let round1: Vec<(usize, Result<crate::search::ShardCandidates>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = shards
                    .iter()
                    .enumerate()
                    .map(|(si, shard)| {
                        let was_missing = initially_missing.contains(&si);
                        scope.spawn(move || {
                            let r = if was_missing {
                                Err(PrismError::NotFound(
                                    "shard was unreachable while pinning the snapshot vector"
                                        .into(),
                                ))
                            } else if shard_is_unreachable(si) {
                                Err(PrismError::NotFound(
                                    "coordinator-boundary fault (injected)".into(),
                                ))
                            } else {
                                shard.candidates(&snaps[si], q)
                            };
                            (si, r)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("a shard round-1 thread panicked"))
                    .collect()
            });

        let mut global: Vec<(f32, String, usize, String, usize)> = Vec::new();
        for (si, r) in round1 {
            match r {
                Ok(cands) => {
                    // Hedge the fragment if the shard is slow — free, because a re-issue against the
                    // pinned snapshot is byte-identical (D-079).
                    maybe_hedge(si, &cands, &mut inflight, &mut hedges, || {
                        shards[si].candidates(&snaps[si], q)
                    })?;
                    // **Fold in the shard's counter contribution.** The overfetch is produced by the
                    // candidate collector inside each shard (§22); a coordinator that built its
                    // counters from scratch reported 0 for every cluster query while the contract
                    // called the number monitored. Summing is the right fold: each shard bounds its
                    // own candidates by `2(1−τ)+ε`, so the query's overfetch is the total the exact τ
                    // will prune back.
                    threshold_overfetch += cands.threshold_overfetch;
                    for cand in cands.candidates {
                        global.push((cand.dist, cand.event_id, si, cand.part_id, cand.row));
                    }
                }
                Err(e) => resolve_unreachable(si, &e.to_string(), q.best_effort, &mut missing)?,
            }
        }
        // Merge by PQ distance, ties on event_id (C-4 across the wire).
        global.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

        // Bound ONCE, globally: rerank width, then the declared byte budget — for the query, not per
        // shard × N. Exhaustion is the same named degradation single-store reports (storage §6).
        // A **threshold** query is bounded by the threshold, not a width: each shard already bounded
        // its candidates to `2(1−τ) + ε` up to the per-shard state budget (D-074), so the coordinator
        // must NOT truncate to `q.rerank` here — that would drop qualifying rows exactly as it would
        // single-store. The byte budget below still holds for the query either way.
        if q.threshold.is_none() {
            global.truncate(q.rerank);
        }
        let mut fetch_budget_exhausted = false;
        if let Some(budget) = q.fetch_budget_bytes {
            let max_vectors = budget / (dim * 4).max(1);
            if global.len() > max_vectors {
                global.truncate(max_vectors);
                fetch_budget_exhausted = true;
            }
        }

        // --- round 2: each owning shard exact-scores its subset of the global set ---
        let mut by_shard: BTreeMap<usize, Vec<(String, usize)>> = BTreeMap::new();
        for (_, _, si, pid, row) in &global {
            by_shard.entry(*si).or_default().push((pid.clone(), *row));
        }
        let mut scored: Vec<Scored> = Vec::new();
        // handle[gidx] = (shard, part_id, row) — how the coordinator routes materialization back.
        let mut handle: Vec<(usize, String, usize)> = Vec::new();
        let mut exact_bytes_fetched = 0usize;
        let mut object_requests = 0usize;

        // Fan out round 2 concurrently too — each owning shard exact-scores its own subset in parallel.
        //
        // The partials fold in **ascending shard id** — canonical order ([query §20](../../../docs/QUERY-CONTRACT.md)) —
        // and the `BTreeMap` gives that for free rather than by convention.
        //
        // **Why nothing observable depends on it, recorded so the dependency is not forgotten.** A
        // mutation that folds these in reverse is *live* — the pre-sort survivor sequence genuinely
        // differs at 2 and 4 shards — and yet is caught by no test, because `finalize` sorts by score
        // and then by the **unique** `event_id`. That comparator is **total**, so arrival order is
        // erased before anything is returned, and the GROUP BY content seed is taken over *sorted*
        // event_ids besides. Canonical folding is therefore a **consequence of C-4 totality**, not an
        // independent guarantee.
        //
        // The wall it rests on is pinned by `the_c4_tie_break_is_total_over_merge_survivors`: if the
        // comparator ever stopped being total, fold order would become observable immediately, and
        // this line would go from redundant to load-bearing.
        let by_shard_vec: Vec<(usize, Vec<(String, usize)>)> = by_shard.into_iter().collect();
        let round2: Vec<Result<Vec<crate::search::ShardScored>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = by_shard_vec
                .iter()
                .map(|(si, sel)| {
                    let si = *si;
                    scope.spawn(move || {
                        if shard_is_unreachable(si) {
                            Err(PrismError::NotFound(
                                "coordinator-boundary fault (injected)".into(),
                            ))
                        } else {
                            shards[si].rerank(&snaps[si], q, sel)
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a shard round-2 thread panicked"))
                .collect()
        });

        for ((si, sel), r) in by_shard_vec.iter().zip(round2) {
            let si = *si;
            let scoreds = match r {
                Ok(s) => s,
                Err(e) => {
                    // A shard reachable in round 1 but not round 2: its candidates simply go unscored
                    // and drop from the answer — recorded missing (or, fail-named, the query errors).
                    resolve_unreachable(si, &e.to_string(), q.best_effort, &mut missing)?;
                    continue;
                }
            };
            // Hedge round 2 too — the exact-score fragment is likewise byte-identical against the
            // pinned snapshot (D-079).
            maybe_hedge(si, &scoreds, &mut inflight, &mut hedges, || {
                shards[si].rerank(&snaps[si], q, sel)
            })?;
            for s in scoreds {
                let gidx = handle.len();
                handle.push((si, s.part_id.clone(), s.row));
                exact_bytes_fetched += s.vector.len() * 4;
                scored.push(Scored {
                    score: s.score,
                    part: gidx,
                    row: s.row,
                    vector: s.vector,
                    event_id: s.event_id,
                });
            }
            object_requests += 1;
        }

        // **Partial results and semantic aggregates do not mix** ([query §21](../../../docs/QUERY-CONTRACT.md)).
        // A best-effort GROUP BY over an incomplete shard set is not comparable to a complete run —
        // cluster mass and exemplars shift with the data present — so returning it flagged invites an
        // analyst to read a partial distribution as a whole one. It is **refused by name** instead.
        refuse_partial_group(q, &missing)?;

        let gen_ids: BTreeSet<String> = snaps
            .iter()
            .filter_map(|snapshot| snapshot.active_generation.clone())
            .collect();
        if gen_ids.len() > 1 {
            return Err(PrismError::Invariant(format!(
                "the pinned snapshot vector serves multiple active generations {gen_ids:?}; \
                 every shard must install the same content-addressed generation before serving"
            )));
        }
        let plan_choice = crate::plan::PlanChoice {
            strategy: crate::plan::Strategy::Interleaved,
            reason: "cluster coordinator (query §20)".into(),
            estimated_selectivity: f64::NAN,
        };
        let c = Counters {
            rerank_width: global.len(),
            fetch_budget_exhausted,
            exact_bytes_fetched,
            object_requests,
            hedges_issued: hedges,
            threshold_overfetch,
            ..Default::default()
        };

        // --- finalize: the SHARED implementation, with a materializer that routes to the shards ---
        //
        // A shard can disappear after exact scoring but before survivor materialization. On the
        // default path this is fail-named. On explicit best-effort, remove every score from that
        // shard and finalize again so the global top-k backfills from reachable shards; returning a
        // short top-k without that re-finalization would be a plausible wrong answer.
        let mut remaining_scored = scored;
        let mut result = loop {
            let materialize_failure: Mutex<Option<(usize, String)>> = Mutex::new(None);
            let materialize =
                |needed: &BTreeSet<(usize, usize)>| -> Result<
                    BTreeMap<(usize, usize), (Event, u32)>,
                > {
                    let mut by_shard_mat: BTreeMap<usize, Vec<(usize, String, usize)>> =
                        BTreeMap::new();
                    for (gidx, row) in needed {
                        let (shard_id, part_id, _) = &handle[*gidx];
                        by_shard_mat.entry(*shard_id).or_default().push((
                            *gidx,
                            part_id.clone(),
                            *row,
                        ));
                    }
                    let mut out: BTreeMap<(usize, usize), (Event, u32)> = BTreeMap::new();
                    for (shard_id, requests) in &by_shard_mat {
                        let selected: Vec<(String, usize)> = requests
                            .iter()
                            .map(|(_, part_id, row)| (part_id.clone(), *row))
                            .collect();
                        let materialized =
                            match shards[*shard_id].materialize(&snaps[*shard_id], &selected) {
                            Ok(materialized) => materialized,
                            Err(error) => {
                                *materialize_failure.lock().expect("materialize failure lock") =
                                    Some((*shard_id, error.to_string()));
                                return Err(PrismError::NotFound(format!(
                                    "shard {shard_id} unreachable during survivor materialization: \
                                     {error}"
                                )));
                            }
                        };
                        for ((global_index, _, row), (event, centroid)) in
                            requests.iter().zip(materialized)
                        {
                            out.insert((*global_index, *row), (event, centroid));
                        }
                    }
                    Ok(out)
                };

            match Engine::finalize(
                dim,
                seed,
                &tombstones,
                &snapshot_id,
                q,
                remaining_scored.clone(),
                &gen_ids,
                &plan_choice,
                c.clone(),
                materialize,
                || 0,
            ) {
                Ok(result) => break result,
                Err(error) => {
                    let failure = materialize_failure
                        .lock()
                        .expect("materialize failure lock")
                        .clone();
                    let Some((shard_id, reason)) = failure else {
                        return Err(error);
                    };
                    resolve_unreachable(shard_id, &reason, q.best_effort, &mut missing)?;
                    refuse_partial_group(q, &missing)?;
                    remaining_scored.retain(|score| handle[score.part].0 != shard_id);
                }
            }
        };
        // Label the partial answer (query §21): the dropped shards and their count, mirrored into the
        // counters so a degraded answer is a monitored number. Only ever non-empty for a best-effort
        // query — a fail-named one errored above — so a partial answer is impossible to mistake for a
        // whole one.
        missing.sort_by_key(|m| m.shard);
        result.counters.shards_missing = missing.len();
        result.missing_shards = missing;
        Ok(result)
    }

    /// A tenant-scoped semantic `GROUP BY`, routed to the owner shard. Cross-tenant clustering needs
    /// the coordinated canonical-shard-order partial merge (the next increment).
    pub fn semantic_cluster(&self, req: &ClusterRequest) -> Result<SemanticClusterResult> {
        self.shards[self.shard_index(&req.tenant)].semantic_cluster(req)
    }
}
