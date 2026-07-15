use prism_types::error::{PrismError, Result};
use prism_types::vector::l2_sq;
use serde::{Deserialize, Serialize};

/// Codewords per sub-quantizer. 256 so a code is exactly one byte — the whole
/// point of the layout is a fixed, byte-aligned stride.
pub const PQ_KSUB: usize = 256;

/// Product quantization codebook: `m` independent sub-quantizers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PqCodebook {
    pub dim: usize,
    /// Number of sub-vectors. `dim` must be divisible by `m`.
    pub m: usize,
    pub sub_dim: usize,
    /// `m * PQ_KSUB * sub_dim` floats.
    pub codewords: Vec<f32>,
}

impl PqCodebook {
    pub fn train(vectors: &[f32], n: usize, dim: usize, m: usize, seed: u64) -> Result<Self> {
        Self::train_restarts(vectors, n, dim, m, seed, crate::kmeans::KMEANS_RESTARTS)
    }

    pub fn train_restarts(
        vectors: &[f32],
        n: usize,
        dim: usize,
        m: usize,
        seed: u64,
        restarts: usize,
    ) -> Result<Self> {
        if m == 0 || dim % m != 0 {
            return Err(PrismError::Invalid(format!(
                "pq_m ({m}) must be positive and divide dim ({dim})"
            )));
        }
        if n == 0 {
            return Err(PrismError::Invalid(
                "cannot train PQ on zero vectors".into(),
            ));
        }
        let sub_dim = dim / m;

        let mut codewords = vec![0.0f32; m * PQ_KSUB * sub_dim];
        for j in 0..m {
            // Gather the j-th slice of every training vector.
            let mut sub = Vec::with_capacity(n * sub_dim);
            for i in 0..n {
                let start = i * dim + j * sub_dim;
                sub.extend_from_slice(&vectors[start..start + sub_dim]);
            }

            // With fewer training points than codewords we train what we can and
            // pad the remainder by repeating the last codeword. The alternative
            // -- inventing random codewords -- would put codes in the table that
            // no encoder can ever emit and quietly distort ADC distances.
            let k = PQ_KSUB.min(n);
            let trained = crate::kmeans::kmeans_restarts(
                &sub,
                n,
                sub_dim,
                k,
                20,
                seed.wrapping_add(j as u64),
                restarts,
            )?;

            let base = j * PQ_KSUB * sub_dim;
            codewords[base..base + k * sub_dim].copy_from_slice(&trained);
            for c in k..PQ_KSUB {
                let last = base + (k - 1) * sub_dim;
                let (head, tail) = codewords.split_at_mut(base + c * sub_dim);
                tail[..sub_dim].copy_from_slice(&head[last..last + sub_dim]);
            }
        }

        Ok(PqCodebook {
            dim,
            m,
            sub_dim,
            codewords,
        })
    }

    #[inline]
    fn codeword(&self, j: usize, c: usize) -> &[f32] {
        let base = (j * PQ_KSUB + c) * self.sub_dim;
        &self.codewords[base..base + self.sub_dim]
    }

    /// Encode one vector to `m` bytes.
    pub fn encode(&self, v: &[f32]) -> Result<Vec<u8>> {
        if v.len() != self.dim {
            return Err(PrismError::Invalid(format!(
                "vector has {} dims, codebook has {}",
                v.len(),
                self.dim
            )));
        }
        let mut code = vec![0u8; self.m];
        for j in 0..self.m {
            let sub = &v[j * self.sub_dim..(j + 1) * self.sub_dim];
            let mut best = 0usize;
            let mut best_d = f32::INFINITY;
            for c in 0..PQ_KSUB {
                let d = l2_sq(sub, self.codeword(j, c));
                if d < best_d {
                    best_d = d;
                    best = c;
                }
            }
            code[j] = best as u8;
        }
        Ok(code)
    }

    /// Reconstruct the lossy approximation of a vector from its code. Used only
    /// by tests measuring quantization error — never by the query path, which
    /// reranks against the *stored exact* vector, not a reconstruction.
    pub fn decode(&self, code: &[u8]) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for j in 0..self.m {
            v[j * self.sub_dim..(j + 1) * self.sub_dim]
                .copy_from_slice(self.codeword(j, code[j] as usize));
        }
        v
    }

    /// Build the asymmetric-distance lookup table for one query vector.
    ///
    /// This is the whole trick: pay `m * 256` sub-distance computations once,
    /// then every row in the scan costs `m` loads and adds against *compressed*
    /// bytes. The scan never touches a float vector.
    pub fn adc_table(&self, q: &[f32]) -> Result<AdcTable> {
        if q.len() != self.dim {
            return Err(PrismError::Invalid(format!(
                "query has {} dims, codebook has {}",
                q.len(),
                self.dim
            )));
        }
        let mut table = vec![0.0f32; self.m * PQ_KSUB];
        for j in 0..self.m {
            let sub = &q[j * self.sub_dim..(j + 1) * self.sub_dim];
            for c in 0..PQ_KSUB {
                table[j * PQ_KSUB + c] = l2_sq(sub, self.codeword(j, c));
            }
        }
        Ok(AdcTable { m: self.m, table })
    }
}

/// A per-query, per-generation distance table.
///
/// One table per generation, always: a code byte means whatever its own
/// codebook says it means, and a table built from another generation's
/// codebook would silently return numbers instead of failing (invariant 9).
#[derive(Clone, Debug)]
pub struct AdcTable {
    pub m: usize,
    table: Vec<f32>,
}

impl AdcTable {
    /// The approximate squared L2 distance between the query and one coded row.
    ///
    /// This is the scalar definition of a single ADC distance
    /// ([docs/DETERMINISM-CONTRACT.md](../../../docs/DETERMINISM-CONTRACT.md) §1.1). The scan
    /// path does not call this per row -- it calls [`crate::kernel::adc_scan`] over a whole range
    /// so a SIMD kernel can process many rows at once -- but every kernel is defined to produce,
    /// for each row, exactly the value this function returns. It is kept as the single-row
    /// reference and used where a lone distance is wanted.
    #[inline]
    pub fn distance(&self, code: &[u8]) -> f32 {
        debug_assert_eq!(code.len(), self.m);
        let mut acc = 0.0f32;
        for (j, &c) in code.iter().enumerate() {
            acc += self.table[j * PQ_KSUB + c as usize];
        }
        acc
    }

    /// The flat `m·256` lookup table, for the batched scan kernels.
    #[inline]
    pub fn table(&self) -> &[f32] {
        &self.table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_types::rng::Rng;
    use prism_types::vector::validate_and_normalize;

    fn corpus(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut rng = Rng::new(seed);
        let mut v = Vec::with_capacity(n * dim);
        // Cluster structure, otherwise PQ has nothing to learn.
        for i in 0..n {
            let cluster = i % 8;
            let mut row: Vec<f32> = (0..dim)
                .map(|j| {
                    let center = if j % 8 == cluster { 3.0 } else { 0.0 };
                    center + rng.normal() * 0.5
                })
                .collect();
            validate_and_normalize(&mut row).unwrap();
            v.extend_from_slice(&row);
        }
        v
    }

    #[test]
    fn rejects_m_that_does_not_divide_dim() {
        let v = corpus(50, 16, 1);
        assert!(PqCodebook::train(&v, 50, 16, 5, 1).is_err());
        assert!(PqCodebook::train(&v, 50, 16, 0, 1).is_err());
    }

    #[test]
    fn code_is_exactly_m_bytes() {
        let v = corpus(300, 16, 2);
        let cb = PqCodebook::train(&v, 300, 16, 4, 2).unwrap();
        let code = cb.encode(&v[..16]).unwrap();
        assert_eq!(code.len(), 4);
    }

    #[test]
    fn adc_distance_equals_distance_to_the_reconstruction() {
        // ADC computes ||q - decode(code)||^2 by parts. Prove the identity
        // holds -- this is what makes the table a valid distance, not a score.
        let v = corpus(400, 16, 3);
        let cb = PqCodebook::train(&v, 400, 16, 4, 3).unwrap();
        let q = &v[..16];
        let table = cb.adc_table(q).unwrap();
        for i in 0..50 {
            let row = &v[i * 16..(i + 1) * 16];
            let code = cb.encode(row).unwrap();
            let adc = table.distance(&code);
            let exact_to_recon = l2_sq(q, &cb.decode(&code));
            assert!(
                (adc - exact_to_recon).abs() < 1e-4,
                "adc={adc} recon={exact_to_recon}"
            );
        }
    }

    #[test]
    fn adc_ranking_approximates_exact_ranking() {
        // The contract PQ actually owes us: the true nearest neighbour survives
        // into a modest candidate list. Not that ADC is exact -- it is not.
        let n = 500;
        let dim = 16;
        let v = corpus(n, dim, 4);
        let cb = PqCodebook::train(&v, n, dim, 8, 4).unwrap();
        let codes: Vec<Vec<u8>> = (0..n)
            .map(|i| cb.encode(&v[i * dim..(i + 1) * dim]).unwrap())
            .collect();

        let mut recovered = 0;
        for qi in 0..20 {
            let q = &v[qi * dim..(qi + 1) * dim];

            let mut exact: Vec<(usize, f32)> = (0..n)
                .map(|i| (i, l2_sq(q, &v[i * dim..(i + 1) * dim])))
                .collect();
            exact.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let truth = exact[0].0;

            let table = cb.adc_table(q).unwrap();
            let mut approx: Vec<(usize, f32)> =
                (0..n).map(|i| (i, table.distance(&codes[i]))).collect();
            approx.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

            if approx[..20].iter().any(|&(i, _)| i == truth) {
                recovered += 1;
            }
        }
        assert!(
            recovered >= 19,
            "true NN survived into top-20 candidates for only {recovered}/20 queries"
        );
    }

    #[test]
    fn training_is_deterministic() {
        let v = corpus(200, 16, 5);
        let a = PqCodebook::train(&v, 200, 16, 4, 9).unwrap();
        let b = PqCodebook::train(&v, 200, 16, 4, 9).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn fewer_points_than_codewords_still_encodes() {
        let v = corpus(10, 8, 6);
        let cb = PqCodebook::train(&v, 10, 8, 2, 6).unwrap();
        let code = cb.encode(&v[..8]).unwrap();
        assert_eq!(code.len(), 2);
        // Padding must be real codewords, so a decode round-trips sanely.
        let d = cb.decode(&code);
        assert_eq!(d.len(), 8);
        assert!(d.iter().all(|x| x.is_finite()));
    }
}
