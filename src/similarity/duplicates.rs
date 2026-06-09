use std::collections::{HashMap, HashSet};

use crate::db::PatternFamily;
use crate::similarity::hrr::{HrrVec, Rng};

// ---------------------------------------------------------------------------
// Union-Find
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
        } else if self.rank[ra] > self.rank[rb] {
            self.parent[rb] = ra;
        } else {
            self.parent[rb] = ra;
            self.rank[ra] += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// SimHash LSH — random hyperplane hashing for cosine similarity
// ---------------------------------------------------------------------------

const LSH_K: usize = 6;
const LSH_L: usize = 12;
const LSH_SEED: u64 = 0xDEAD_BEEF_CAFE_1234;
const BRUTE_FORCE_THRESHOLD: usize = 50;

struct SimHashIndex {
    tables: Vec<HashMap<u64, Vec<usize>>>,
    hyperplanes: Vec<Vec<Vec<f64>>>,
}

impl SimHashIndex {
    fn new(dim: usize) -> Self {
        let mut rng = Rng::new(LSH_SEED);
        let hyperplanes: Vec<Vec<Vec<f64>>> = (0..LSH_L)
            .map(|_| {
                (0..LSH_K)
                    .map(|_| (0..dim).map(|_| rng.next_gaussian()).collect())
                    .collect()
            })
            .collect();
        Self {
            tables: (0..LSH_L).map(|_| HashMap::new()).collect(),
            hyperplanes,
        }
    }

    fn hash_vector(&self, vec: &[f64], table_idx: usize) -> u64 {
        let mut hash = 0u64;
        for (bit, plane) in self.hyperplanes[table_idx].iter().enumerate() {
            let dot: f64 = vec.iter().zip(plane).map(|(a, b)| a * b).sum();
            if dot >= 0.0 {
                hash |= 1 << bit;
            }
        }
        hash
    }

    fn insert(&mut self, idx: usize, vec: &[f64]) {
        for t in 0..self.tables.len() {
            let h = self.hash_vector(vec, t);
            self.tables[t].entry(h).or_default().push(idx);
        }
    }

    fn candidate_pairs(&self) -> HashSet<(usize, usize)> {
        let mut pairs = HashSet::new();
        for table in &self.tables {
            for bucket in table.values() {
                if bucket.len() < 2 {
                    continue;
                }
                for i in 0..bucket.len() {
                    for j in (i + 1)..bucket.len() {
                        let (a, b) = if bucket[i] < bucket[j] {
                            (bucket[i], bucket[j])
                        } else {
                            (bucket[j], bucket[i])
                        };
                        pairs.insert((a, b));
                    }
                }
            }
        }
        pairs
    }
}

// ---------------------------------------------------------------------------
// Similarity cache — canonical key ordering, reused across all phases
// ---------------------------------------------------------------------------

struct SimCache {
    cache: HashMap<(usize, usize), f64>,
}

impl SimCache {
    fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    fn get_or_compute(&mut self, i: usize, j: usize, vecs: &[(i64, HrrVec)]) -> f64 {
        let key = if i < j { (i, j) } else { (j, i) };
        *self
            .cache
            .entry(key)
            .or_insert_with(|| vecs[key.0].1.dot_product(&vecs[key.1].1))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn find_pattern_families(
    vectors: &[(i64, HrrVec)],
    threshold: f64,
    min_group: usize,
) -> Vec<PatternFamily> {
    let n = vectors.len();
    if n < min_group {
        return Vec::new();
    }

    let normalized: Vec<(i64, HrrVec)> =
        vectors.iter().map(|(id, v)| (*id, v.normalize())).collect();

    let mut cache = SimCache::new();
    let mut uf = UnionFind::new(n);

    // Phase 1: candidate generation + union-find
    if n <= BRUTE_FORCE_THRESHOLD {
        for i in 0..n {
            for j in (i + 1)..n {
                let sim = cache.get_or_compute(i, j, &normalized);
                if sim >= threshold {
                    uf.union(i, j);
                }
            }
        }
    } else {
        let dim = normalized[0].1.data.len();
        let mut index = SimHashIndex::new(dim);
        for (i, (_, v)) in normalized.iter().enumerate() {
            index.insert(i, &v.data);
        }
        for (i, j) in index.candidate_pairs() {
            let sim = cache.get_or_compute(i, j, &normalized);
            if sim >= threshold {
                uf.union(i, j);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }

    // Phase 2: complete-link pruning — remove members until all pairs pass
    let mut families = Vec::new();
    for (_root, mut members) in groups {
        if members.len() < min_group {
            continue;
        }

        loop {
            let mut worst_idx = None;
            let mut worst_min = f64::MAX;
            let mut all_pass = true;

            for (mi, &a) in members.iter().enumerate() {
                let mut min_sim = f64::MAX;
                for &b in &members {
                    if a == b {
                        continue;
                    }
                    let sim = cache.get_or_compute(a, b, &normalized);
                    min_sim = min_sim.min(sim);
                }
                if min_sim < threshold {
                    all_pass = false;
                    if min_sim < worst_min {
                        worst_min = min_sim;
                        worst_idx = Some(mi);
                    }
                }
            }

            if all_pass {
                break;
            }
            if let Some(idx) = worst_idx {
                members.swap_remove(idx);
            }
            if members.len() < min_group {
                break;
            }
        }

        if members.len() < min_group {
            continue;
        }

        // Phase 3: avg similarity for surviving family
        let mut sim_sum = 0.0;
        let mut pair_count = 0u64;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                sim_sum += cache.get_or_compute(members[i], members[j], &normalized);
                pair_count += 1;
            }
        }

        families.push(PatternFamily {
            member_symbol_ids: members.iter().map(|&i| normalized[i].0).collect(),
            avg_similarity: if pair_count > 0 {
                sim_sum / pair_count as f64
            } else {
                0.0
            },
        });
    }

    families.sort_by(|a, b| b.member_symbol_ids.len().cmp(&a.member_symbol_ids.len()));
    families
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::similarity::hrr::{HrrVec, Rng};

    fn make_vec(seed: u64) -> HrrVec {
        let mut rng = Rng::new(seed);
        HrrVec::random(&mut rng)
    }

    #[test]
    fn identical_vectors_form_family() {
        let v = make_vec(42);
        let vectors = vec![(1, v.clone()), (2, v.clone()), (3, v.clone())];
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].member_symbol_ids.len(), 3);
        assert!(families[0].avg_similarity > 0.99);
    }

    #[test]
    fn below_threshold_no_cluster() {
        let vectors = vec![(1, make_vec(1)), (2, make_vec(2)), (3, make_vec(3))];
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert!(families.is_empty());
    }

    #[test]
    fn small_groups_filtered() {
        let v = make_vec(42);
        let vectors = vec![(1, v.clone()), (2, v.clone()), (3, make_vec(99))];
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert!(families.is_empty());
    }

    #[test]
    fn empty_input() {
        let families = find_pattern_families(&[], 0.85, 3);
        assert!(families.is_empty());
    }

    #[test]
    fn custom_min_group() {
        let v = make_vec(42);
        let vectors = vec![(1, v.clone()), (2, v.clone())];
        let families = find_pattern_families(&vectors, 0.85, 2);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].member_symbol_ids.len(), 2);
    }

    #[test]
    fn transitive_chain_pruned() {
        // A≈B and B≈C but A≉C — single-link would group all three, complete-link should not.
        let a = make_vec(100);
        let b = make_vec(200);
        let c = make_vec(300);

        let mut sims = [
            (a.cosine_similarity(&b), "A-B"),
            (b.cosine_similarity(&c), "B-C"),
            (a.cosine_similarity(&c), "A-C"),
        ];
        sims.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());

        // threshold between the weakest pair and the second-weakest
        let threshold = (sims[0].0 + sims[1].0) / 2.0;
        assert!(
            sims[0].0 < threshold && sims[1].0 >= threshold,
            "need a gap between weakest and second-weakest pair"
        );

        let vectors = vec![(1, a), (2, b), (3, c)];
        let families = find_pattern_families(&vectors, threshold, 3);
        assert!(
            families.is_empty(),
            "transitive chain should not form a family of 3 \
             (weakest pair {}={:.4} < threshold={:.4}), got {} families",
            sims[0].1,
            sims[0].0,
            threshold,
            families.len()
        );
    }

    #[test]
    fn multiple_families() {
        let v1 = make_vec(10);
        let v2 = make_vec(20);
        let vectors = vec![
            (1, v1.clone()),
            (2, v1.clone()),
            (3, v1.clone()),
            (4, v2.clone()),
            (5, v2.clone()),
            (6, v2.clone()),
        ];
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert_eq!(families.len(), 2);
    }
}
