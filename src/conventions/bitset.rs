use std::fmt;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    pub fn new(len: usize) -> Self {
        let nwords = len.div_ceil(64);
        Self {
            words: vec![0; nwords],
            len,
        }
    }

    pub fn from_indices(len: usize, indices: impl IntoIterator<Item = usize>) -> Self {
        let mut bs = Self::new(len);
        for i in indices {
            bs.set(i);
        }
        bs
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1u64 << (i % 64);
    }

    pub fn contains(&self, i: usize) -> bool {
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    pub fn intersect(&self, other: &Self) -> Self {
        debug_assert_eq!(
            self.len, other.len,
            "BitSet::intersect on different lengths"
        );
        let mut r = self.clone();
        for (w, o) in r.words.iter_mut().zip(&other.words) {
            *w &= *o;
        }
        r
    }

    pub fn is_subset_of(&self, other: &Self) -> bool {
        debug_assert_eq!(
            self.len, other.len,
            "BitSet::is_subset_of on different lengths"
        );
        self.words
            .iter()
            .zip(&other.words)
            .all(|(a, b)| *a & *b == *a)
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(|&i| self.contains(i))
    }
}

impl fmt::Debug for BitSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        let mut first = true;
        for i in self.iter() {
            if !first {
                write!(f, ", ")?;
            }
            write!(f, "{i}")?;
            first = false;
        }
        write!(f, "}}")
    }
}
