use std::collections::HashMap;

use super::hrr::{HrrVec, Rng};

pub struct Codebook {
    entries: HashMap<String, HrrVec>,
    new_keys: Vec<String>,
    rng: Rng,
}

impl Codebook {
    pub fn from_entries(entries: HashMap<String, HrrVec>) -> Self {
        let seed = 0x5554_5241 ^ (entries.len() as u64);
        Self {
            entries,
            new_keys: Vec::new(),
            rng: Rng::new(seed),
        }
    }

    pub fn get_or_create(&mut self, key: &str) -> HrrVec {
        if let Some(v) = self.entries.get(key) {
            return v.clone();
        }
        let v = HrrVec::random(&mut self.rng);
        self.entries.insert(key.to_string(), v.clone());
        self.new_keys.push(key.to_string());
        v
    }

    pub fn into_new_entries(self) -> Vec<(String, Vec<u8>)> {
        let Self {
            entries, new_keys, ..
        } = self;
        new_keys
            .into_iter()
            .map(|key| {
                let blob = entries[&key].to_bytes();
                (key, blob)
            })
            .collect()
    }

    #[cfg(test)]
    pub fn new_empty(seed: u64) -> Self {
        Self {
            entries: HashMap::new(),
            new_keys: Vec::new(),
            rng: Rng::new(seed),
        }
    }
}
