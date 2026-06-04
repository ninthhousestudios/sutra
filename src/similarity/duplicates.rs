use std::collections::HashMap;

use crate::db::PatternFamily;
use crate::similarity::hrr::HrrVec;

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

pub fn find_pattern_families(
    vectors: &[(i64, HrrVec)],
    threshold: f64,
    min_group: usize,
) -> Vec<PatternFamily> {
    let n = vectors.len();
    if n < min_group {
        return Vec::new();
    }

    let mut uf = UnionFind::new(n);
    let mut pairwise_sims: Vec<(usize, usize, f64)> = Vec::new();

    for i in 0..n {
        for j in (i + 1)..n {
            let sim = vectors[i].1.cosine_similarity(&vectors[j].1);
            if sim >= threshold {
                uf.union(i, j);
                pairwise_sims.push((i, j, sim));
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }

    let mut families: Vec<PatternFamily> = groups
        .into_values()
        .filter(|members| members.len() >= min_group)
        .map(|members| {
            let mut sim_sum = 0.0;
            let mut sim_count = 0u64;
            for &(a, b, sim) in &pairwise_sims {
                if members.contains(&a) && members.contains(&b) {
                    sim_sum += sim;
                    sim_count += 1;
                }
            }
            let avg_similarity = if sim_count > 0 {
                sim_sum / sim_count as f64
            } else {
                0.0
            };
            PatternFamily {
                member_symbol_ids: members.iter().map(|&i| vectors[i].0).collect(),
                avg_similarity,
            }
        })
        .collect();

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
        let vectors = vec![
            (1, make_vec(1)),
            (2, make_vec(2)),
            (3, make_vec(3)),
        ];
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
