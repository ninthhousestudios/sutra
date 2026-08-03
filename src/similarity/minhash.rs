use std::collections::HashMap;
use std::hash::{Hash, Hasher};

const MERSENNE_PRIME: u64 = (1 << 61) - 1;
const HASH_MASK: u64 = 0xFFFF_FFFF;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub struct MinHash {
    hash_values: Vec<u64>,
    a: Vec<u64>,
    b: Vec<u64>,
}

impl MinHash {
    pub fn new(num_perm: usize, seed: u64) -> Self {
        let mut rng = SplitMix64::new(seed);
        let a: Vec<u64> = (0..num_perm)
            .map(|_| (rng.next() % (MERSENNE_PRIME - 1)) + 1)
            .collect();
        let b: Vec<u64> = (0..num_perm).map(|_| rng.next() % MERSENNE_PRIME).collect();
        Self {
            hash_values: vec![HASH_MASK; num_perm],
            a,
            b,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        let hv = fnv1a(data) & HASH_MASK;
        for i in 0..self.hash_values.len() {
            let phv =
                ((self.a[i].wrapping_mul(hv).wrapping_add(self.b[i])) % MERSENNE_PRIME) & HASH_MASK;
            if phv < self.hash_values[i] {
                self.hash_values[i] = phv;
            }
        }
    }

    pub fn hash_values(&self) -> &[u64] {
        &self.hash_values
    }

    pub fn jaccard(&self, other: &MinHash) -> f64 {
        let matching = self
            .hash_values
            .iter()
            .zip(other.hash_values.iter())
            .filter(|(a, b)| a == b)
            .count();
        matching as f64 / self.hash_values.len() as f64
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

pub struct MinHashLSH {
    rows: usize,
    tables: Vec<HashMap<u64, Vec<usize>>>,
}

impl MinHashLSH {
    pub fn new(threshold: f64, num_perm: usize) -> Self {
        let (bands, rows) = optimal_params(threshold, num_perm);
        Self {
            rows,
            tables: (0..bands).map(|_| HashMap::new()).collect(),
        }
    }

    pub fn insert(&mut self, idx: usize, minhash: &MinHash) {
        let hv = minhash.hash_values();
        for (band_idx, table) in self.tables.iter_mut().enumerate() {
            let start = band_idx * self.rows;
            let end = (start + self.rows).min(hv.len());
            let band_hash = hash_band(&hv[start..end]);
            table.entry(band_hash).or_default().push(idx);
        }
    }

    pub fn candidate_pairs(&self) -> Vec<(usize, usize)> {
        const MAX_BUCKET: usize = 200;
        let mut seen = std::collections::HashSet::new();
        let mut pairs = Vec::new();
        for table in &self.tables {
            for bucket in table.values() {
                if bucket.len() < 2 {
                    continue;
                }
                let effective = if bucket.len() <= MAX_BUCKET {
                    &bucket[..]
                } else {
                    &bucket[..MAX_BUCKET]
                };
                for i in 0..effective.len() {
                    for j in (i + 1)..effective.len() {
                        let (a, b) = if effective[i] < effective[j] {
                            (effective[i], effective[j])
                        } else {
                            (effective[j], effective[i])
                        };
                        if seen.insert((a, b)) {
                            pairs.push((a, b));
                        }
                    }
                }
            }
        }
        pairs
    }
}

fn hash_band(band: &[u64]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    band.hash(&mut hasher);
    hasher.finish()
}

fn optimal_params(threshold: f64, num_perm: usize) -> (usize, usize) {
    let mut best_err = f64::MAX;
    let mut best = (1, 1);
    for b in 1..=num_perm {
        let r = num_perm / b;
        if r == 0 {
            break;
        }
        let fp = integrate(
            |s| 1.0 - (1.0 - s.powi(r as i32)).powi(b as i32),
            0.0,
            threshold,
        );
        let fnn = integrate(|s| (1.0 - s.powi(r as i32)).powi(b as i32), threshold, 1.0);
        let err = 0.5 * fp + 0.5 * fnn;
        if err < best_err {
            best_err = err;
            best = (b, r);
        }
    }
    best
}

fn integrate<F: Fn(f64) -> f64>(f: F, lo: f64, hi: f64) -> f64 {
    let n = 128;
    let h = (hi - lo) / n as f64;
    (0..n).map(|i| h * f(lo + i as f64 * h)).sum()
}

pub fn shingles(text: &str, k: usize) -> Vec<&str> {
    let char_indices: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    if char_indices.len() < k {
        return vec![text];
    }
    (0..=char_indices.len() - k)
        .map(|i| {
            let start = char_indices[i];
            let end = if i + k < char_indices.len() {
                char_indices[i + k]
            } else {
                text.len()
            };
            &text[start..end]
        })
        .collect()
}

pub fn shannon_entropy(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let mut freq = [0u32; 256];
    let mut n = 0u32;
    for b in text.bytes() {
        freq[b as usize] += 1;
        n += 1;
    }
    let nf = n as f64;
    freq.iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / nf;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_have_jaccard_one() {
        let mut a = MinHash::new(128, 42);
        let mut b = MinHash::new(128, 42);
        for s in shingles("process_items", 3) {
            a.update(s.as_bytes());
            b.update(s.as_bytes());
        }
        assert!((a.jaccard(&b) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn different_texts_have_low_jaccard() {
        let mut a = MinHash::new(128, 42);
        let mut b = MinHash::new(128, 42);
        for s in shingles("process_items", 3) {
            a.update(s.as_bytes());
        }
        for s in shingles("completely_different_name", 3) {
            b.update(s.as_bytes());
        }
        assert!(a.jaccard(&b) < 0.3);
    }

    #[test]
    fn similar_texts_have_moderate_jaccard() {
        let mut a = MinHash::new(128, 42);
        let mut b = MinHash::new(128, 42);
        for s in shingles("processitems", 3) {
            a.update(s.as_bytes());
        }
        for s in shingles("processitem", 3) {
            b.update(s.as_bytes());
        }
        assert!(a.jaccard(&b) > 0.5);
    }

    #[test]
    fn entropy_low_for_short_names() {
        assert!(shannon_entropy("ab") < 2.5);
        assert!(shannon_entropy("M1") < 2.5);
    }

    #[test]
    fn entropy_adequate_for_real_names() {
        assert!(shannon_entropy("process_items") > 2.5);
        assert!(shannon_entropy("handle_request") > 2.5);
    }

    #[test]
    fn lsh_finds_similar_pair() {
        let mut mh1 = MinHash::new(128, 42);
        let mut mh2 = MinHash::new(128, 42);
        let mut mh3 = MinHash::new(128, 42);
        for s in shingles("processitems", 3) {
            mh1.update(s.as_bytes());
        }
        for s in shingles("processitem", 3) {
            mh2.update(s.as_bytes());
        }
        for s in shingles("totallyunrelated", 3) {
            mh3.update(s.as_bytes());
        }

        let mut lsh = MinHashLSH::new(0.4, 128);
        lsh.insert(0, &mh1);
        lsh.insert(1, &mh2);
        lsh.insert(2, &mh3);

        let pairs = lsh.candidate_pairs();
        assert!(pairs.contains(&(0, 1)));
        assert!(!pairs.contains(&(0, 2)));
    }
}
