use std::collections::HashMap;

use super::hrr::{HrrVec, Rng};

/// Content-addressed codebook: every key maps to a pseudo-random vector whose
/// RNG seed is a stable hash of the key itself, so the same key yields the
/// same vector on any machine, in any encounter order, with nothing persisted
/// (sutra/327). The map is a per-run memo, not a source of truth — dropping it
/// changes nothing but speed.
pub struct Codebook {
    cache: HashMap<String, HrrVec>,
}

/// FNV-1a 64-bit — stable across runs and platforms, unlike SipHash.
/// Collisions merely make two keys share a vector; at ~10⁵ keys the
/// probability is ~1e-10 and the effect degrades gracefully.
fn stable_hash64(key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Codebook {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    pub fn get_or_create(&mut self, key: &str) -> HrrVec {
        if let Some(v) = self.cache.get(key) {
            return v.clone();
        }
        let mut rng = Rng::new(stable_hash64(key));
        let v = HrrVec::random(&mut rng);
        self.cache.insert(key.to_string(), v.clone());
        v
    }
}

impl Default for Codebook {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_same_vector_across_instances() {
        let mut a = Codebook::new();
        let mut b = Codebook::new();
        assert_eq!(
            a.get_or_create("identifier").data,
            b.get_or_create("identifier").data
        );
    }

    #[test]
    fn encounter_order_does_not_change_assignment() {
        let mut a = Codebook::new();
        let first_then_second = (
            a.get_or_create("first").data.clone(),
            a.get_or_create("second").data.clone(),
        );
        let mut b = Codebook::new();
        let second_then_first = (
            b.get_or_create("second").data.clone(),
            b.get_or_create("first").data.clone(),
        );
        assert_eq!(first_then_second.0, second_then_first.1);
        assert_eq!(first_then_second.1, second_then_first.0);
    }

    #[test]
    fn different_keys_different_vectors() {
        let mut cb = Codebook::new();
        let a = cb.get_or_create("alpha");
        let b = cb.get_or_create("beta");
        let sim = a.cosine_similarity(&b);
        assert!(sim.abs() < 0.15, "sim={sim}");
    }
}
