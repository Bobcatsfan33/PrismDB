//! The S9 labeled-cluster corpus generator (directive 5).
//!
//! `semantic_cluster` needs an oracle, and the honest oracle for clustering is **ground truth**:
//! synthetic rows whose true cluster is known, so `ARI(ours, truth)` is a real number and not a
//! comparison of one approximation against another. (PRISM.md names sklearn; we have no
//! dependencies and no need of one — labeled synthetics *are* the exact answer sklearn would only
//! estimate.)
//!
//! The corpus is frozen under [C-2](../../../docs/DECISIONS.md) in `testing/cluster/v1/`. This
//! module is the generator the freeze step runs; the gate reads the committed bytes, never this.
//!
//! Four shapes, because a clusterer that only works on round, equal, well-separated blobs is a
//! clusterer that only works in the demo:
//!
//! - **balanced** — equal, separated topics. The easy case, the ARI floor should be comfortable.
//! - **zipf** — the same topics at Zipf-skewed sizes (one big, a long tail of small). Unequal
//!   masses are where a naive k-means splits the big cluster and merges the small ones.
//! - **overlap** — topics with blurred boundaries: a third of rows borrow a neighbouring topic's
//!   phrase, so the clusters genuinely touch. ARI must stay honest, not perfect.
//! - **noise** — uniform token soup, no structure at all. The honest answer here is **not** *k*
//!   confident clusters; it is *low confidence*, and the gate asserts exactly that.

use prism_types::attributes::Attributes;
use prism_types::rng::Rng;
use prism_types::Event;

const BASE_TIME: i64 = 1_760_000_000_000;

/// The number of true clusters in the labeled corpus.
pub const TRUE_CLUSTERS: usize = 8;
/// Distinct tokens in each cluster's private vocabulary.
const VOCAB_PER_CLUSTER: usize = 6;
/// Tokens per generated row.
const TOKENS_PER_ROW: usize = 5;

/// A token from cluster `c`'s private vocabulary. Clusters share **no** vocabulary, so their
/// bag-of-features embeddings occupy distinct regions and k-means can recover them — this is a
/// *clustering* oracle, not an NLP benchmark, so clean separation is the point (directive 5).
fn cluster_token(c: usize, w: usize) -> String {
    format!("kw{c}x{w}")
}

/// Draw `TOKENS_PER_ROW` tokens from cluster `c`'s vocabulary into a body.
fn cluster_body(rng: &mut Rng, c: usize) -> String {
    let mut b = String::new();
    for t in 0..TOKENS_PER_ROW {
        if t > 0 {
            b.push(' ');
        }
        b.push_str(&cluster_token(c, rng.below(VOCAB_PER_CLUSTER)));
    }
    b
}

/// The adversarial shapes. The label of every row (except `Noise`) is its topic, carried in
/// `event_name`; the ARI gate maps that back to a class index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Balanced,
    Zipf,
    Overlap,
    Noise,
}

impl Shape {
    pub fn name(self) -> &'static str {
        match self {
            Shape::Balanced => "balanced",
            Shape::Zipf => "zipf",
            Shape::Overlap => "overlap",
            Shape::Noise => "noise",
        }
    }

    pub fn parse(s: &str) -> Option<Shape> {
        match s {
            "balanced" => Some(Shape::Balanced),
            "zipf" => Some(Shape::Zipf),
            "overlap" => Some(Shape::Overlap),
            "noise" => Some(Shape::Noise),
            _ => None,
        }
    }

    pub const ALL: [Shape; 4] = [Shape::Balanced, Shape::Zipf, Shape::Overlap, Shape::Noise];
}

/// Generate `rows` labeled events for `shape`, deterministically from `seed`. The true cluster of
/// each row is carried in `event_name` as `c{index}` (or `noise`); [`true_label`] reads it back.
pub fn generate(shape: Shape, rows: usize, seed: u64) -> Vec<Event> {
    let mut rng = Rng::new(seed);
    let clusters = TRUE_CLUSTERS;
    let zipf: Vec<f32> = (1..=clusters).map(|r| 1.0 / r as f32).collect();
    let zipf_total: f32 = zipf.iter().sum();

    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        let c = match shape {
            Shape::Zipf => {
                let mut target = rng.next_f32() * zipf_total;
                let mut t = clusters - 1;
                for (r, w) in zipf.iter().enumerate() {
                    target -= w;
                    if target <= 0.0 {
                        t = r;
                        break;
                    }
                }
                t
            }
            _ => rng.below(clusters),
        };

        let (body, label) = match shape {
            Shape::Noise => {
                // No structure: every token drawn from *every* cluster's vocabulary uniformly, so
                // the embeddings scatter and no `k` describes them better than one blob does.
                let mut b = String::new();
                for t in 0..TOKENS_PER_ROW {
                    if t > 0 {
                        b.push(' ');
                    }
                    let cc = rng.below(clusters);
                    b.push_str(&cluster_token(cc, rng.below(VOCAB_PER_CLUSTER)));
                }
                (b, "noise".to_string())
            }
            Shape::Overlap => {
                // A third of rows borrow a neighbouring cluster's tokens, blurring the boundary,
                // but keep their own label — the clusters genuinely touch.
                let mut b = cluster_body(&mut rng, c);
                if rng.next_f32() < 0.33 {
                    let neighbour = (c + 1) % clusters;
                    b = format!(
                        "{b} {} {}",
                        cluster_token(neighbour, rng.below(VOCAB_PER_CLUSTER)),
                        cluster_token(neighbour, rng.below(VOCAB_PER_CLUSTER))
                    );
                }
                (b, format!("c{c}"))
            }
            _ => (cluster_body(&mut rng, c), format!("c{c}")),
        };

        let cost = (rng.next_f32() * 0.05) as f64;
        let error = rng.next_f32() < 0.12;

        out.push(Event {
            event_id: format!("e{i:08}"),
            tenant_id: "alpha".to_string(),
            event_time: BASE_TIME + i as i64 * 1000,
            observed_time: BASE_TIME + i as i64 * 1000,
            event_name: label,
            cost,
            error,
            body,
            trace_id: format!("{:032x}", 0x5eed_0000_0000_0000u64 ^ (i as u64)),
            span_id: format!("{:016x}", 0xabc0_0000u64 ^ (i as u64)),
            attributes: Attributes::new(),
            idempotency_key: None,
        });
    }
    out
}

/// Injected-novelty events for the drift benchmark: `n` rows of a genuinely **new** behaviour —
/// `class` names a token namespace (`nv{class}…`) that shares nothing with the labeled corpus's
/// `kw…` vocabulary, so these rows are far from any baseline built on it. Each class is its own
/// tight cluster, so a per-class (tail) recall is meaningful. Label is `nv{class}`.
pub fn injected_novel(class: usize, n: usize, seed: u64) -> Vec<Event> {
    // Sparse and distinctive: few tokens per row from a small private vocabulary, so a novel
    // event's vector is nearly orthogonal to the dense baseline centroids and its novelty is
    // reliably high. (More tokens make the vector denser and *closer* to everything — the wrong
    // direction for "far from known structure".)
    const NOVEL_VOCAB: usize = 4;
    const NOVEL_TOKENS: usize = 3;
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|i| {
            let mut body = String::new();
            for t in 0..NOVEL_TOKENS {
                if t > 0 {
                    body.push(' ');
                }
                body.push_str(&format!("nv{class}t{}", rng.below(NOVEL_VOCAB)));
            }
            Event {
                event_id: format!("nv{class}_{i:06}"),
                tenant_id: "alpha".to_string(),
                event_time: BASE_TIME + i as i64 * 1000,
                observed_time: BASE_TIME + i as i64 * 1000,
                event_name: format!("nv{class}"),
                cost: (rng.next_f32() * 0.05) as f64,
                error: rng.next_f32() < 0.12,
                body,
                trace_id: format!("{:032x}", 0x4004_0000_0000_0000u64 ^ (i as u64)),
                span_id: format!("{:016x}", 0xdef0_0000u64 ^ (i as u64)),
                attributes: Attributes::new(),
                idempotency_key: None,
            }
        })
        .collect()
}

/// The true class index of a labeled event, for ARI. `Noise` rows share one class (there is no
/// structure to recover), every other row's class is the `c{index}` its `event_name` carries.
pub fn true_label(event: &Event) -> usize {
    if event.event_name == "noise" {
        return usize::MAX;
    }
    event
        .event_name
        .strip_prefix('c')
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(usize::MAX)
}

/// The **Adjusted Rand Index** of a clustering against ground-truth labels — hand-rolled, because
/// the charter forbids the dependency that would supply it, and ARI is a formula, not a library.
///
/// ARI ∈ [−1, 1]: 1 is a perfect match up to relabeling, 0 is chance. Computed from the pair
/// confusion via the contingency table, so it is invariant to how each side numbers its clusters —
/// which is exactly what makes it the right metric for an ephemeral-id clustering (§15).
pub fn adjusted_rand_index(predicted: &[usize], truth: &[usize]) -> f64 {
    assert_eq!(predicted.len(), truth.len());
    let n = predicted.len();
    if n == 0 {
        return 1.0;
    }
    use std::collections::BTreeMap;
    let mut table: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    let mut a: BTreeMap<usize, u64> = BTreeMap::new();
    let mut b: BTreeMap<usize, u64> = BTreeMap::new();
    for i in 0..n {
        *table.entry((predicted[i], truth[i])).or_default() += 1;
        *a.entry(predicted[i]).or_default() += 1;
        *b.entry(truth[i]).or_default() += 1;
    }
    let comb2 = |x: u64| -> f64 { (x as f64) * (x as f64 - 1.0) / 2.0 };
    let sum_ij: f64 = table.values().map(|&c| comb2(c)).sum();
    let sum_a: f64 = a.values().map(|&c| comb2(c)).sum();
    let sum_b: f64 = b.values().map(|&c| comb2(c)).sum();
    let total = comb2(n as u64);
    let expected = sum_a * sum_b / total;
    let max = 0.5 * (sum_a + sum_b);
    if (max - expected).abs() < f64::EPSILON {
        return 1.0; // both trivially one cluster: perfect agreement
    }
    (sum_ij - expected) / (max - expected)
}
