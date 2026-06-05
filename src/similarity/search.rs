use super::hrr::HrrVec;

pub struct SimilarityMatch {
    pub symbol_id: i64,
    pub score: f64,
}

pub fn find_similar(
    query_id: i64,
    query_vec: &HrrVec,
    candidates: &[(i64, HrrVec)],
    limit: usize,
    threshold: f64,
) -> Vec<SimilarityMatch> {
    let mut matches: Vec<SimilarityMatch> = candidates
        .iter()
        .filter(|(id, _)| *id != query_id)
        .filter_map(|(id, vec)| {
            let score = query_vec.cosine_similarity(vec);
            if score >= threshold {
                Some(SimilarityMatch {
                    symbol_id: *id,
                    score,
                })
            } else {
                None
            }
        })
        .collect();

    matches.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    matches.truncate(limit);
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::similarity::hrr::Rng;

    fn make_vec(seed: u64) -> HrrVec {
        let mut rng = Rng::new(seed);
        HrrVec::random(&mut rng)
    }

    #[test]
    fn ranked_results() {
        let query = make_vec(1);
        let close = query.normalize();
        let far = make_vec(99);
        let candidates = vec![(10, close.clone()), (20, far)];
        let results = find_similar(1, &query, &candidates, 10, 0.0);
        assert_eq!(results.len(), 2);
        assert!(results[0].score > results[1].score);
        assert_eq!(results[0].symbol_id, 10);
    }

    #[test]
    fn self_exclusion() {
        let query = make_vec(1);
        let candidates = vec![(1, query.clone()), (2, query.clone())];
        let results = find_similar(1, &query, &candidates, 10, 0.0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol_id, 2);
    }

    #[test]
    fn threshold_filtering() {
        let query = make_vec(1);
        let far = make_vec(99);
        let candidates = vec![(10, far)];
        let results = find_similar(1, &query, &candidates, 10, 0.99);
        assert!(results.is_empty());
    }

    #[test]
    fn limit_truncation() {
        let query = make_vec(1);
        let candidates: Vec<(i64, HrrVec)> = (10..20)
            .map(|i| (i, query.normalize()))
            .collect();
        let results = find_similar(1, &query, &candidates, 3, 0.0);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn empty_candidates() {
        let query = make_vec(1);
        let results = find_similar(1, &query, &[], 10, 0.0);
        assert!(results.is_empty());
    }
}
