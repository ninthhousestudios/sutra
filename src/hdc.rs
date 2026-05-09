use std::collections::HashMap;

const DEFAULT_DIM: usize = 10_000;
const WORDS: usize = (DEFAULT_DIM + 63) / 64; // 157 u64s

#[derive(Clone)]
pub struct HdcVec {
    pub bits: Vec<u64>,
    pub dim: usize,
}

impl HdcVec {
    pub fn random(rng: &mut Rng) -> Self {
        let mut bits: Vec<u64> = (0..WORDS).map(|_| rng.next_u64()).collect();
        let tail = DEFAULT_DIM % 64;
        if tail != 0 {
            bits[WORDS - 1] &= (1u64 << tail) - 1;
        }
        Self { bits, dim: DEFAULT_DIM }
    }

    pub fn zero() -> Self {
        Self { bits: vec![0u64; WORDS], dim: DEFAULT_DIM }
    }

    pub fn bind(&self, other: &Self) -> Self {
        let bits = self.bits.iter().zip(&other.bits).map(|(a, b)| a ^ b).collect();
        Self { bits, dim: self.dim }
    }

    /// Circular left-shift by `n` bits — encodes sequential position.
    pub fn permute(&self, n: usize) -> Self {
        let n = n % self.dim;
        if n == 0 {
            return self.clone();
        }
        let mut out = vec![0u64; WORDS];
        for i in 0..self.dim {
            let src_word = i / 64;
            let src_bit = i % 64;
            let dst = (i + n) % self.dim;
            let dst_word = dst / 64;
            let dst_bit = dst % 64;
            if (self.bits[src_word] >> src_bit) & 1 == 1 {
                out[dst_word] |= 1u64 << dst_bit;
            }
        }
        Self { bits: out, dim: self.dim }
    }

    pub fn hamming(&self, other: &Self) -> u32 {
        self.bits.iter().zip(&other.bits).map(|(a, b)| (a ^ b).count_ones()).sum()
    }

    pub fn cosine_similarity(&self, other: &Self) -> f64 {
        let dist = self.hamming(other) as f64;
        1.0 - 2.0 * dist / self.dim as f64
    }
}

impl std::fmt::Debug for HdcVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ones: u32 = self.bits.iter().map(|w| w.count_ones()).sum();
        write!(f, "HdcVec(dim={}, ones={})", self.dim, ones)
    }
}

/// Majority-vote bundling: for each bit position, output 1 if more than half
/// the input vectors have 1, else 0. Tie-breaks randomly.
pub fn bundle(vecs: &[HdcVec], rng: &mut Rng) -> HdcVec {
    assert!(!vecs.is_empty());
    if vecs.len() == 1 {
        return vecs[0].clone();
    }
    let threshold = vecs.len() as u32 / 2;
    let tie_needs_break = vecs.len() % 2 == 0;
    let mut out = vec![0u64; WORDS];

    for bit in 0..DEFAULT_DIM {
        let word = bit / 64;
        let mask = 1u64 << (bit % 64);
        let count: u32 = vecs.iter().map(|v| ((v.bits[word] & mask) != 0) as u32).sum();
        let set = if tie_needs_break && count == threshold {
            rng.next_u64() & 1 == 1
        } else {
            count > threshold
        };
        if set {
            out[word] |= mask;
        }
    }
    HdcVec { bits: out, dim: DEFAULT_DIM }
}

/// Simple xoshiro128-based RNG — good enough for generating base vectors.
pub struct Rng {
    s: [u64; 2],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { s: [seed ^ 0x5555555555555555, seed.wrapping_mul(6364136223846793005)] }
    }

    pub fn next_u64(&mut self) -> u64 {
        let s0 = self.s[0];
        let mut s1 = self.s[1];
        let result = s0.wrapping_add(s1);
        s1 ^= s0;
        self.s[0] = s0.rotate_left(24) ^ s1 ^ (s1 << 16);
        self.s[1] = s1.rotate_left(37);
        result
    }
}

/// Maps tree-sitter node kinds to random base vectors.
pub struct Codebook {
    pub entries: HashMap<String, HdcVec>,
    rng: Rng,
}

impl Codebook {
    pub fn new(seed: u64) -> Self {
        Self {
            entries: HashMap::new(),
            rng: Rng::new(seed),
        }
    }

    pub fn get_or_create(&mut self, kind: &str) -> HdcVec {
        if let Some(v) = self.entries.get(kind) {
            return v.clone();
        }
        let v = HdcVec::random(&mut self.rng);
        self.entries.insert(kind.to_string(), v.clone());
        v
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentMode {
    /// Pure structural — identifiers encoded only by their node kind
    Strip,
    /// Encode actual identifier text into the vector
    Embed,
}

/// Encode a tree-sitter node into an HDC vector recursively.
/// `ident_mode` controls whether identifier text is embedded (semantic search)
/// or stripped (pure structural search).
pub fn encode(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    rng: &mut Rng,
    max_depth: usize,
    ident_mode: IdentMode,
) -> HdcVec {
    let kind_vec = codebook.get_or_create(node.kind());

    if max_depth == 0 || node.child_count() == 0 {
        if ident_mode == IdentMode::Embed
            && (node.kind() == "identifier" || node.kind() == "type_identifier")
        {
            if let Ok(text) = node.utf8_text(source) {
                let name_vec = codebook.get_or_create(&format!("id:{text}"));
                return kind_vec.bind(&name_vec);
            }
        }
        return kind_vec;
    }

    let mut child_vecs = Vec::with_capacity(node.child_count());
    let mut pos = 0usize;
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        if !child.is_named() {
            continue;
        }
        let child_enc = encode(&child, source, codebook, rng, max_depth - 1, ident_mode);
        let positional = child_enc.permute(pos + 1);
        child_vecs.push(positional);
        pos += 1;
    }

    if child_vecs.is_empty() {
        return kind_vec;
    }

    let bundled_children = bundle(&child_vecs, rng);
    kind_vec.bind(&bundled_children)
}

/// Convenience: encode with identifiers embedded
pub fn encode_structural(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    rng: &mut Rng,
    max_depth: usize,
) -> HdcVec {
    encode(node, source, codebook, rng, max_depth, IdentMode::Embed)
}

/// Build a prototype vector from a group (just bundle, but semantically distinct).
pub fn prototype(vecs: &[HdcVec], rng: &mut Rng) -> HdcVec {
    bundle(vecs, rng)
}

/// Unbind: recover the "other" component from a composite.
/// Algebraically identical to bind (XOR is self-inverse), but named for clarity.
pub fn unbind(composite: &HdcVec, known: &HdcVec) -> HdcVec {
    composite.bind(known)
}

/// Best-match lookup against a codebook — find which entry the noisy vector
/// is closest to. Returns (label, similarity).
pub fn cleanup(noisy: &HdcVec, codebook: &Codebook) -> Option<(String, f64)> {
    codebook.entries.iter()
        .map(|(label, vec)| (label.clone(), noisy.cosine_similarity(vec)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
}

// --- Sequence / control-flow encoding ---

/// An operation extracted from linearized control flow.
#[derive(Debug, Clone)]
pub struct Op {
    pub label: String,
    pub line: usize,
}

/// Extract a linearized sequence of operations from a function body.
/// Walks statements in source order, classifying each by its primary effect.
pub fn extract_ops(node: &tree_sitter::Node, source: &[u8]) -> Vec<Op> {
    let mut ops = Vec::new();
    extract_ops_inner(node, source, &mut ops);
    ops
}

fn extract_ops_inner(node: &tree_sitter::Node, source: &[u8], ops: &mut Vec<Op>) {
    let line = node.start_position().row;
    match node.kind() {
        // C operations
        "call_expression" => {
            let label = node.child_by_field_name("function")
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or("call");
            ops.push(Op { label: classify_call(label), line });
            return; // don't recurse into call's children
        }
        "return_statement" | "return_expression" => {
            ops.push(Op { label: "return".into(), line });
            return;
        }
        "if_statement" | "if_expression" => {
            ops.push(Op { label: "branch".into(), line });
        }
        "for_statement" | "for_expression" | "while_statement"
        | "while_expression" | "loop_expression" | "do_statement" => {
            ops.push(Op { label: "loop".into(), line });
        }
        "switch_statement" | "match_expression" => {
            ops.push(Op { label: "switch".into(), line });
        }
        "goto_statement" => {
            ops.push(Op { label: "goto".into(), line });
            return;
        }
        "assignment_expression" => {
            ops.push(Op { label: "assign".into(), line });
        }
        "pointer_expression" => {
            ops.push(Op { label: "deref".into(), line });
        }
        "try_expression" => {
            ops.push(Op { label: "try".into(), line });
        }
        "unsafe_block" => {
            ops.push(Op { label: "unsafe".into(), line });
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        extract_ops_inner(&node.child(i).unwrap(), source, ops);
    }
}

fn classify_call(name: &str) -> String {
    let base = name.rsplit("::").next().unwrap_or(name);
    let base = base.rsplit("->").next().unwrap_or(base);
    let base = base.rsplit('.').next().unwrap_or(base);
    match base {
        "malloc" | "calloc" | "realloc" | "kmalloc" | "kzalloc"
        | "vmalloc" | "krealloc" | "kcalloc" | "alloc" => "alloc".into(),

        "free" | "kfree" | "vfree" | "kfree_rcu" => "free".into(),

        "mutex_lock" | "spin_lock" | "spin_lock_irqsave" | "raw_spin_lock"
        | "read_lock" | "write_lock" | "down" | "down_read" | "down_write"
        | "lock" => "lock".into(),

        "mutex_unlock" | "spin_unlock" | "spin_unlock_irqrestore" | "raw_spin_unlock"
        | "read_unlock" | "write_unlock" | "up" | "up_read" | "up_write"
        | "unlock" => "unlock".into(),

        "memcpy" | "memmove" | "memset" | "copy_from_user" | "copy_to_user"
        | "strncpy" | "strlcpy" => "memop".into(),

        "printk" | "pr_err" | "pr_warn" | "pr_info" | "pr_debug"
        | "dev_err" | "dev_warn" | "dev_info" | "println" | "eprintln"
        | "tracing" => "log".into(),

        "IS_ERR" | "PTR_ERR" | "ERR_PTR" | "is_ok" | "is_err"
        | "is_none" | "is_some" => "errcheck".into(),

        "unwrap" | "expect" | "BUG" | "BUG_ON" | "WARN_ON" | "panic" => "panic".into(),

        _ => format!("call:{base}"),
    }
}

/// Encode a sequence of operations as bigram vectors.
/// Each consecutive pair (op_i, op_{i+1}) is encoded as bind(op_i, op_{i+1}.permute(1)).
/// All bigrams are bundled into a single vector.
pub fn encode_bigrams(ops: &[Op], codebook: &mut Codebook, rng: &mut Rng) -> Option<HdcVec> {
    if ops.len() < 2 {
        return None;
    }
    let bigrams: Vec<HdcVec> = ops.windows(2)
        .map(|w| {
            let a = codebook.get_or_create(&format!("op:{}", w[0].label));
            let b = codebook.get_or_create(&format!("op:{}", w[1].label));
            a.bind(&b.permute(1))
        })
        .collect();
    Some(bundle(&bigrams, rng))
}

/// Encode the full operation sequence with absolute position.
pub fn encode_sequence(ops: &[Op], codebook: &mut Codebook, rng: &mut Rng) -> Option<HdcVec> {
    if ops.is_empty() {
        return None;
    }
    let positional: Vec<HdcVec> = ops.iter().enumerate()
        .map(|(i, op)| {
            let v = codebook.get_or_create(&format!("op:{}", op.label));
            v.permute(i + 1)
        })
        .collect();
    Some(bundle(&positional, rng))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_vectors_are_roughly_half_ones() {
        let mut rng = Rng::new(42);
        let v = HdcVec::random(&mut rng);
        let ones: u32 = v.bits.iter().map(|w| w.count_ones()).sum();
        // should be ~5000 +/- 200
        assert!((4500..5500).contains(&ones), "ones={ones}");
    }

    #[test]
    fn bind_is_self_inverse() {
        let mut rng = Rng::new(42);
        let a = HdcVec::random(&mut rng);
        let b = HdcVec::random(&mut rng);
        let bound = a.bind(&b);
        let recovered = bound.bind(&b);
        assert_eq!(recovered.hamming(&a), 0);
    }

    #[test]
    fn random_vectors_are_roughly_orthogonal() {
        let mut rng = Rng::new(42);
        let a = HdcVec::random(&mut rng);
        let b = HdcVec::random(&mut rng);
        let sim = a.cosine_similarity(&b);
        assert!(sim.abs() < 0.1, "sim={sim}");
    }

    #[test]
    fn bundle_preserves_similarity() {
        let mut rng = Rng::new(42);
        let a = HdcVec::random(&mut rng);
        let b = HdcVec::random(&mut rng);
        let c = HdcVec::random(&mut rng);
        let bundled = bundle(&[a.clone(), b.clone(), c.clone()], &mut rng);
        // bundled should be more similar to each component than random
        let sim_a = bundled.cosine_similarity(&a);
        let sim_b = bundled.cosine_similarity(&b);
        let sim_c = bundled.cosine_similarity(&c);
        assert!(sim_a > 0.2, "sim_a={sim_a}");
        assert!(sim_b > 0.2, "sim_b={sim_b}");
        assert!(sim_c > 0.2, "sim_c={sim_c}");
    }

    #[test]
    fn permute_preserves_density() {
        let mut rng = Rng::new(42);
        let v = HdcVec::random(&mut rng);
        let p = v.permute(7);
        let ones_v: u32 = v.bits.iter().map(|w| w.count_ones()).sum();
        let ones_p: u32 = p.bits.iter().map(|w| w.count_ones()).sum();
        assert_eq!(ones_v, ones_p);
        // permuted should be roughly orthogonal to original
        let sim = v.cosine_similarity(&p);
        assert!(sim.abs() < 0.15, "sim={sim}");
    }
}
