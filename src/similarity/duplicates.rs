use std::collections::{HashMap, HashSet};

use crate::db::PatternFamily;
use crate::similarity::hrr::{HrrVec, Rng};
use crate::similarity::minhash::{self, MinHash, MinHashLSH};

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

const LSH_K: usize = 10;
const LSH_L: usize = 24;
const LSH_SEED: u64 = 0xDEAD_BEEF_CAFE_1234;
const BRUTE_FORCE_THRESHOLD: usize = 50;
const MAX_BUCKET_SIZE: usize = 200;
const MAX_GROUP_SIZE: usize = 1000;
/// Upper bound on the working set fed to the O(members²) core-extraction
/// ranking. Union-find can transitively merge thousands of near-identical
/// symbols (e.g. decompiled C functions, unioned via LSH star-buckets) into a
/// single group; ranking all-pairs over ~15k members fills SimCache with ~10⁸
/// entries and OOMs (sutra/324). Pre-truncating bounds the ranking cost; the
/// survivors are still reduced to MAX_GROUP_SIZE, and a family of thousands of
/// clones is boilerplate noise regardless of which members are kept.
const CORE_EXTRACTION_CAP: usize = 2000;

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
                if bucket.len() <= MAX_BUCKET_SIZE {
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
                } else {
                    // Star pattern: pair every element with the first,
                    // giving O(B) pairs with full coverage. Union-find
                    // transitively connects truly similar elements;
                    // complete-link pruning rejects false positives.
                    let hub = bucket[0];
                    for &spoke in &bucket[1..] {
                        let (a, b) = if hub < spoke {
                            (hub, spoke)
                        } else {
                            (spoke, hub)
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

        // For oversized groups, keep the most connected members by
        // average similarity to all others (greedy core extraction).
        if members.len() > MAX_GROUP_SIZE {
            // Bound the O(members²) ranking below: a pathologically large
            // group (thousands of near-identical clones) would otherwise fill
            // SimCache with ~10⁸ pairs and OOM (sutra/324). Deterministically
            // pre-truncate the working set first.
            if members.len() > CORE_EXTRACTION_CAP {
                members.sort_unstable();
                members.truncate(CORE_EXTRACTION_CAP);
            }
            let mut avg_sims: Vec<(usize, f64)> = members
                .iter()
                .enumerate()
                .map(|(mi, &a)| {
                    let sum: f64 = members
                        .iter()
                        .filter(|&&b| b != a)
                        .map(|&b| cache.get_or_compute(a, b, &normalized))
                        .sum();
                    (mi, sum / (members.len() - 1) as f64)
                })
                .collect();
            avg_sims.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let keep: HashSet<usize> = avg_sims[..MAX_GROUP_SIZE]
                .iter()
                .map(|&(mi, _)| mi)
                .collect();
            members = members
                .into_iter()
                .enumerate()
                .filter(|(mi, _)| keep.contains(mi))
                .map(|(_, v)| v)
                .collect();
        }

        loop {
            let mut failing: Vec<(usize, f64)> = Vec::new();

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
                    failing.push((mi, min_sim));
                }
            }

            if failing.is_empty() {
                break;
            }

            // Remove the worst half of failing members per iteration,
            // but never drop below min_group in one pass
            failing.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let max_removable = members.len().saturating_sub(min_group);
            if max_removable == 0 {
                // At min_group with unresolved failures — group is invalid
                members.clear();
                break;
            }
            let to_remove = (failing.len() / 2).max(1).min(max_removable);
            let mut remove_indices: Vec<usize> =
                failing[..to_remove].iter().map(|&(i, _)| i).collect();
            remove_indices.sort_unstable_by(|a, b| b.cmp(a));
            for idx in remove_indices {
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
            detection_mode: "structural",
        });
    }

    families.sort_by_key(|f| std::cmp::Reverse(f.member_symbol_ids.len()));
    families
}

// ---------------------------------------------------------------------------
// Name-based duplicate detection (MinHash/LSH)
// ---------------------------------------------------------------------------

const MINHASH_NUM_PERM: usize = 128;
const MINHASH_SEED: u64 = 0xCAFE_BABE_DEAD_BEEF;
const MINHASH_LSH_THRESHOLD: f64 = 0.4;
const ENTROPY_THRESHOLD: f64 = 2.5;
const SHINGLE_K: usize = 3;

fn normalize_name(qualified: &str) -> String {
    let parts: Vec<&str> = qualified.rsplitn(3, "::").collect();
    let short = match parts.len() {
        1 => parts[0],
        2 => return format!("{}::{}", parts[1], parts[0]),
        _ => return format!("{}::{}", parts[1], parts[0]),
    };
    short.to_string()
}

fn name_to_shingle_text(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn dice_from_jaccard(jaccard: f64) -> f64 {
    (2.0 * jaccard) / (1.0 + jaccard)
}

fn exact_dice(a: &str, b: &str) -> f64 {
    let sa: HashSet<&str> = minhash::shingles(a, SHINGLE_K).into_iter().collect();
    let sb: HashSet<&str> = minhash::shingles(b, SHINGLE_K).into_iter().collect();
    let intersection = sa.intersection(&sb).count();
    let total = sa.len() + sb.len();
    if total == 0 {
        return 0.0;
    }
    (2 * intersection) as f64 / total as f64
}

pub fn find_name_families(
    symbols: &[(i64, String)],
    threshold: f64,
    min_group: usize,
) -> Vec<PatternFamily> {
    if symbols.len() < min_group {
        return Vec::new();
    }

    let prepared: Vec<(usize, i64, String)> = symbols
        .iter()
        .enumerate()
        .filter_map(|(idx, (id, qname))| {
            let norm = normalize_name(qname);
            let text = name_to_shingle_text(&norm);
            if text.len() < SHINGLE_K {
                return None;
            }
            if minhash::shannon_entropy(&text) < ENTROPY_THRESHOLD {
                return None;
            }
            Some((idx, *id, text))
        })
        .collect();

    if prepared.len() < min_group {
        return Vec::new();
    }

    let minhashes: Vec<MinHash> = prepared
        .iter()
        .map(|(_, _, text)| {
            let mut mh = MinHash::new(MINHASH_NUM_PERM, MINHASH_SEED);
            for s in minhash::shingles(text, SHINGLE_K) {
                mh.update(s.as_bytes());
            }
            mh
        })
        .collect();

    let n = prepared.len();
    let mut uf = UnionFind::new(n);

    if n <= BRUTE_FORCE_THRESHOLD {
        for i in 0..n {
            for j in (i + 1)..n {
                let jaccard = minhashes[i].jaccard(&minhashes[j]);
                if dice_from_jaccard(jaccard) >= threshold {
                    let dice = exact_dice(&prepared[i].2, &prepared[j].2);
                    if dice >= threshold {
                        uf.union(i, j);
                    }
                }
            }
        }
    } else {
        let mut lsh = MinHashLSH::new(MINHASH_LSH_THRESHOLD, MINHASH_NUM_PERM);
        for (i, mh) in minhashes.iter().enumerate() {
            lsh.insert(i, mh);
        }
        for (i, j) in lsh.candidate_pairs() {
            let jaccard = minhashes[i].jaccard(&minhashes[j]);
            if dice_from_jaccard(jaccard) >= threshold {
                let dice = exact_dice(&prepared[i].2, &prepared[j].2);
                if dice >= threshold {
                    uf.union(i, j);
                }
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        groups.entry(uf.find(i)).or_default().push(i);
    }

    // Complete-link pruning: remove members until all pairs meet threshold
    let mut families = Vec::new();
    for (_root, mut members) in groups {
        if members.len() < min_group {
            continue;
        }

        if members.len() > MAX_GROUP_SIZE {
            let mut avg_sims: Vec<(usize, f64)> = members
                .iter()
                .enumerate()
                .map(|(mi, &a)| {
                    let sum: f64 = members
                        .iter()
                        .filter(|&&b| b != a)
                        .map(|&b| exact_dice(&prepared[a].2, &prepared[b].2))
                        .sum();
                    (mi, sum / (members.len() - 1) as f64)
                })
                .collect();
            avg_sims.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let keep: HashSet<usize> = avg_sims[..MAX_GROUP_SIZE]
                .iter()
                .map(|&(mi, _)| mi)
                .collect();
            members = members
                .into_iter()
                .enumerate()
                .filter(|(mi, _)| keep.contains(mi))
                .map(|(_, v)| v)
                .collect();
        }

        loop {
            let mut failing: Vec<(usize, f64)> = Vec::new();
            for (mi, &a) in members.iter().enumerate() {
                let mut min_sim = f64::MAX;
                for &b in &members {
                    if a == b {
                        continue;
                    }
                    let sim = exact_dice(&prepared[a].2, &prepared[b].2);
                    min_sim = min_sim.min(sim);
                }
                if min_sim < threshold {
                    failing.push((mi, min_sim));
                }
            }

            if failing.is_empty() {
                break;
            }

            failing.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let max_removable = members.len().saturating_sub(min_group);
            if max_removable == 0 {
                members.clear();
                break;
            }
            let to_remove = (failing.len() / 2).max(1).min(max_removable);
            let mut remove_indices: Vec<usize> =
                failing[..to_remove].iter().map(|&(i, _)| i).collect();
            remove_indices.sort_unstable_by(|a, b| b.cmp(a));
            for idx in remove_indices {
                members.swap_remove(idx);
            }

            if members.len() < min_group {
                break;
            }
        }

        if members.len() < min_group {
            continue;
        }

        let mut sim_sum = 0.0;
        let mut pair_count = 0u64;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                sim_sum += exact_dice(&prepared[members[i]].2, &prepared[members[j]].2);
                pair_count += 1;
            }
        }
        families.push(PatternFamily {
            member_symbol_ids: members.iter().map(|&i| prepared[i].1).collect(),
            avg_similarity: if pair_count > 0 {
                sim_sum / pair_count as f64
            } else {
                0.0
            },
            detection_mode: "name",
        });
    }

    families.sort_by_key(|f| std::cmp::Reverse(f.member_symbol_ids.len()));
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

    #[test]
    fn clique_plus_outlier_preserves_family() {
        // 3 identical vectors plus 1 random outlier — the clique must survive
        let v = make_vec(42);
        let outlier = make_vec(999);
        let vectors = vec![(1, v.clone()), (2, v.clone()), (3, v.clone()), (4, outlier)];
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert_eq!(
            families.len(),
            1,
            "clique of 3 should survive with 1 outlier"
        );
        assert_eq!(families[0].member_symbol_ids.len(), 3);
    }

    #[test]
    fn large_identical_family_not_dropped() {
        // Family larger than BRUTE_FORCE_THRESHOLD should still be detected
        let v = make_vec(42);
        let n = 250;
        let vectors: Vec<(i64, HrrVec)> = (0..n).map(|i| (i as i64, v.clone())).collect();
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert_eq!(
            families.len(),
            1,
            "large identical family should not be dropped"
        );
        assert_eq!(families[0].member_symbol_ids.len(), n);
    }

    #[test]
    fn name_families_similar_names_grouped() {
        let symbols = vec![
            (1, "Module::process_items".to_string()),
            (2, "Module::process_item".to_string()),
            (3, "Module::process_itemz".to_string()),
        ];
        let families = find_name_families(&symbols, 0.6, 3);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].member_symbol_ids.len(), 3);
        assert_eq!(families[0].detection_mode, "name");
    }

    #[test]
    fn name_families_different_names_no_group() {
        let symbols = vec![
            (1, "Alpha::compute_metrics".to_string()),
            (2, "Beta::serialize_response".to_string()),
            (3, "Gamma::validate_input".to_string()),
        ];
        let families = find_name_families(&symbols, 0.6, 3);
        assert!(families.is_empty());
    }

    #[test]
    fn name_families_entropy_gate_filters_short() {
        let symbols = vec![
            (1, "A::new".to_string()),
            (2, "B::new".to_string()),
            (3, "C::new".to_string()),
        ];
        let families = find_name_families(&symbols, 0.6, 3);
        assert!(
            families.is_empty(),
            "short low-entropy names should be filtered"
        );
    }

    #[test]
    fn name_families_empty_input() {
        let families = find_name_families(&[], 0.6, 3);
        assert!(families.is_empty());
    }

    #[test]
    fn name_families_min_group_respected() {
        let symbols = vec![
            (1, "Module::process_items".to_string()),
            (2, "Module::process_item".to_string()),
        ];
        let families = find_name_families(&symbols, 0.6, 3);
        assert!(families.is_empty());
        let families = find_name_families(&symbols, 0.6, 2);
        assert_eq!(families.len(), 1);
    }

    #[test]
    fn structural_families_tagged_correctly() {
        let v = make_vec(42);
        let vectors = vec![(1, v.clone()), (2, v.clone()), (3, v.clone())];
        let families = find_pattern_families(&vectors, 0.85, 3);
        assert_eq!(families[0].detection_mode, "structural");
    }

    #[test]
    fn name_families_transitive_chain_pruned() {
        let symbols = vec![
            (1, "Module::abcdefghijkl".to_string()),
            (2, "Module::abcdefghijklqwertyuiopas".to_string()),
            (3, "Module::qwertyuiopas".to_string()),
        ];
        let families = find_name_families(&symbols, 0.6, 3);
        assert!(
            families.is_empty(),
            "transitive chain with dissimilar endpoints should be pruned"
        );
    }

    #[test]
    fn name_families_unicode_identifiers() {
        let symbols = vec![
            (1, "Module::процесс_данных".to_string()),
            (2, "Module::процесс_данные".to_string()),
            (3, "Module::процесс_данным".to_string()),
        ];
        let families = find_name_families(&symbols, 0.5, 3);
        assert_eq!(
            families.len(),
            1,
            "Unicode names should be shingled correctly"
        );
    }
}
