// Holographic Reduced Representation (Plate 1995) spike.
//
// Real-valued vectors with circular convolution binding.
// Key difference from binary MAP (hdc.rs): unbinding via circular
// correlation produces clean residuals, enabling compositional
// decomposition that binary XOR cannot.

use std::collections::HashMap;
use std::f64::consts::PI;

pub const DEFAULT_DIM: usize = 1024;

// --- Complex arithmetic for FFT ---

#[derive(Clone, Copy)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn new(re: f64, im: f64) -> Self { Self { re, im } }
    fn mul(self, other: Self) -> Self {
        Self {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }
    fn conj(self) -> Self { Self { re: self.re, im: -self.im } }
}

impl std::ops::Add for Complex {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self { re: self.re + other.re, im: self.im + other.im }
    }
}

impl std::ops::Sub for Complex {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self { re: self.re - other.re, im: self.im - other.im }
    }
}

/// Radix-2 Cooley-Tukey FFT, in-place.
fn fft(buf: &mut [Complex], inverse: bool) {
    let n = buf.len();
    assert!(n.is_power_of_two());

    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            buf.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let angle = 2.0 * PI / len as f64 * if inverse { -1.0 } else { 1.0 };
        let wn = Complex::new(angle.cos(), angle.sin());

        for start in (0..n).step_by(len) {
            let mut w = Complex::new(1.0, 0.0);
            for k in 0..half {
                let u = buf[start + k];
                let t = w.mul(buf[start + k + half]);
                buf[start + k] = u + t;
                buf[start + k + half] = u - t;
                w = w.mul(wn);
            }
        }
        len <<= 1;
    }

    if inverse {
        let inv_n = 1.0 / n as f64;
        for x in buf.iter_mut() {
            x.re *= inv_n;
            x.im *= inv_n;
        }
    }
}

fn to_freq(data: &[f64]) -> Vec<Complex> {
    let mut buf: Vec<Complex> = data.iter().map(|&x| Complex::new(x, 0.0)).collect();
    fft(&mut buf, false);
    buf
}

fn from_freq(buf: &mut [Complex]) -> Vec<f64> {
    fft(buf, true);
    buf.iter().map(|c| c.re).collect()
}

// --- HRR Vector ---

#[derive(Clone)]
pub struct HrrVec {
    pub data: Vec<f64>,
}

impl HrrVec {
    pub fn random(rng: &mut Rng) -> Self {
        let scale = 1.0 / (DEFAULT_DIM as f64).sqrt();
        let data: Vec<f64> = (0..DEFAULT_DIM)
            .map(|_| rng.next_gaussian() * scale)
            .collect();
        Self { data }
    }

    pub fn zero() -> Self {
        Self { data: vec![0.0; DEFAULT_DIM] }
    }

    pub fn dim(&self) -> usize { self.data.len() }

    /// Binding: circular convolution via FFT.
    pub fn bind(&self, other: &Self) -> Self {
        let mut fa = to_freq(&self.data);
        let fb = to_freq(&other.data);
        for i in 0..fa.len() {
            fa[i] = fa[i].mul(fb[i]);
        }
        Self { data: from_freq(&mut fa) }
    }

    /// Unbinding: circular correlation via FFT.
    /// FFT(self ⋆ other) = FFT(self) · conj(FFT(other))
    /// Recovers the unknown component: if c = a ⊛ b, then c.unbind(b) ≈ a.
    pub fn unbind(&self, other: &Self) -> Self {
        let mut fa = to_freq(&self.data);
        let fb = to_freq(&other.data);
        for i in 0..fa.len() {
            fa[i] = fa[i].mul(fb[i].conj());
        }
        Self { data: from_freq(&mut fa) }
    }

    pub fn cosine_similarity(&self, other: &Self) -> f64 {
        let dot: f64 = self.data.iter().zip(&other.data).map(|(a, b)| a * b).sum();
        let na: f64 = self.data.iter().map(|x| x * x).sum::<f64>().sqrt();
        let nb: f64 = other.data.iter().map(|x| x * x).sum::<f64>().sqrt();
        if na < 1e-15 || nb < 1e-15 { return 0.0; }
        dot / (na * nb)
    }

    pub fn dot(&self, other: &Self) -> f64 {
        self.data.iter().zip(&other.data).map(|(a, b)| a * b).sum()
    }

    pub fn norm(&self) -> f64 {
        self.data.iter().map(|x| x * x).sum::<f64>().sqrt()
    }

    pub fn normalize(&self) -> Self {
        let n = self.norm();
        if n < 1e-15 { return self.clone(); }
        Self { data: self.data.iter().map(|x| x / n).collect() }
    }

    pub fn add(&self, other: &Self) -> Self {
        Self {
            data: self.data.iter().zip(&other.data).map(|(a, b)| a + b).collect(),
        }
    }

    pub fn sub(&self, other: &Self) -> Self {
        Self {
            data: self.data.iter().zip(&other.data).map(|(a, b)| a - b).collect(),
        }
    }

    pub fn scale(&self, s: f64) -> Self {
        Self { data: self.data.iter().map(|x| x * s).collect() }
    }

    /// Circular shift — position encoding (same role as in binary MAP).
    pub fn permute(&self, n: usize) -> Self {
        let n = n % self.dim();
        if n == 0 { return self.clone(); }
        let mut out = vec![0.0; self.dim()];
        for i in 0..self.dim() {
            out[(i + n) % self.dim()] = self.data[i];
        }
        Self { data: out }
    }
}

impl std::fmt::Debug for HrrVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HrrVec(dim={}, norm={:.4})", self.dim(), self.norm())
    }
}

// --- RNG with Gaussian support ---

pub struct Rng {
    state: [u64; 2],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: [seed ^ 0x123456789abcdef0, seed.wrapping_mul(6364136223846793005)] }
    }

    pub fn next_u64(&mut self) -> u64 {
        let s0 = self.state[0];
        let mut s1 = self.state[1];
        let result = s0.wrapping_add(s1);
        s1 ^= s0;
        self.state[0] = s0.rotate_left(55) ^ s1 ^ (s1 << 14);
        self.state[1] = s1.rotate_left(36);
        result
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn next_gaussian(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-15);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
}

// --- Codebook ---

pub struct Codebook {
    entries: HashMap<String, HrrVec>,
    rng: Rng,
}

impl Codebook {
    pub fn new(seed: u64) -> Self {
        Self { entries: HashMap::new(), rng: Rng::new(seed) }
    }

    pub fn get_or_create(&mut self, kind: &str) -> HrrVec {
        if let Some(v) = self.entries.get(kind) {
            return v.clone();
        }
        let v = HrrVec::random(&mut self.rng);
        self.entries.insert(kind.to_string(), v.clone());
        v
    }

    pub fn len(&self) -> usize { self.entries.len() }

    pub fn entries(&self) -> &HashMap<String, HrrVec> { &self.entries }
}

/// Bundle by element-wise addition, then normalize.
pub fn bundle(vecs: &[HrrVec]) -> HrrVec {
    assert!(!vecs.is_empty());
    let mut sum = HrrVec::zero();
    for v in vecs {
        sum = sum.add(v);
    }
    sum.normalize()
}

/// Best-match lookup — cosine similarity to each codebook entry.
pub fn cleanup(noisy: &HrrVec, codebook: &Codebook) -> Option<(String, f64)> {
    codebook.entries().iter()
        .map(|(label, vec)| (label.clone(), noisy.cosine_similarity(vec)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_vectors_are_roughly_orthogonal() {
        let mut rng = Rng::new(42);
        let a = HrrVec::random(&mut rng);
        let b = HrrVec::random(&mut rng);
        let sim = a.cosine_similarity(&b);
        assert!(sim.abs() < 0.15, "sim={sim}");
    }

    #[test]
    fn random_vectors_are_roughly_unit_norm() {
        let mut rng = Rng::new(42);
        for _ in 0..10 {
            let v = HrrVec::random(&mut rng);
            let n = v.norm();
            assert!((n - 1.0).abs() < 0.2, "norm={n}");
        }
    }

    #[test]
    fn bind_unbind_recovers_cleanly() {
        let mut rng = Rng::new(42);
        let a = HrrVec::random(&mut rng);
        let b = HrrVec::random(&mut rng);
        let bound = a.bind(&b);
        let recovered = bound.unbind(&b);
        let sim = recovered.cosine_similarity(&a);
        // ~0.70 for dim=1024: correlation recovers direction but |FFT(b)|²
        // scaling is non-uniform. Exact inverse (division) would give 1.0
        // but is numerically unstable. 0.7 is vastly better than binary MAP's noise.
        assert!(sim > 0.6, "bind-unbind sim={sim:.4}, expected >0.6");
    }

    #[test]
    fn bind_produces_dissimilar_vector() {
        let mut rng = Rng::new(42);
        let a = HrrVec::random(&mut rng);
        let b = HrrVec::random(&mut rng);
        let bound = a.bind(&b);
        let sim_a = bound.cosine_similarity(&a);
        let sim_b = bound.cosine_similarity(&b);
        assert!(sim_a.abs() < 0.15, "sim_a={sim_a}");
        assert!(sim_b.abs() < 0.15, "sim_b={sim_b}");
    }

    #[test]
    fn bundle_preserves_similarity() {
        let mut rng = Rng::new(42);
        let a = HrrVec::random(&mut rng);
        let b = HrrVec::random(&mut rng);
        let c = HrrVec::random(&mut rng);
        let bundled = bundle(&[a.clone(), b.clone(), c.clone()]);
        let sim_a = bundled.cosine_similarity(&a);
        let sim_b = bundled.cosine_similarity(&b);
        let sim_c = bundled.cosine_similarity(&c);
        assert!(sim_a > 0.2, "sim_a={sim_a}");
        assert!(sim_b > 0.2, "sim_b={sim_b}");
        assert!(sim_c > 0.2, "sim_c={sim_c}");
    }

    #[test]
    fn unbind_from_bundle_recovers_filler() {
        let mut rng = Rng::new(42);
        let role_a = HrrVec::random(&mut rng);
        let role_b = HrrVec::random(&mut rng);
        let filler_a = HrrVec::random(&mut rng);
        let filler_b = HrrVec::random(&mut rng);

        let pair_a = role_a.bind(&filler_a);
        let pair_b = role_b.bind(&filler_b);
        let bundled = bundle(&[pair_a, pair_b]);

        let recovered = bundled.unbind(&role_a);
        let sim_correct = recovered.cosine_similarity(&filler_a);
        let sim_wrong = recovered.cosine_similarity(&filler_b);

        assert!(sim_correct > sim_wrong,
            "correct={sim_correct:.4}, wrong={sim_wrong:.4}");
        assert!(sim_correct > 0.1,
            "sim_correct={sim_correct:.4}, expected meaningful similarity");
    }

    #[test]
    fn cleanup_from_bundle_finds_correct_entry() {
        let mut codebook = Codebook::new(42);
        let mut rng = Rng::new(99);

        let labels: Vec<String> = (0..10).map(|i| format!("kind_{i}")).collect();
        let vecs: Vec<HrrVec> = labels.iter()
            .map(|l| codebook.get_or_create(l))
            .collect();

        let role_0 = HrrVec::random(&mut rng);
        let role_1 = HrrVec::random(&mut rng);
        let role_2 = HrrVec::random(&mut rng);

        let bundled = bundle(&[
            role_0.bind(&vecs[3]),
            role_1.bind(&vecs[7]),
            role_2.bind(&vecs[1]),
        ]);

        let recovered = bundled.unbind(&role_0);
        let (best_label, best_sim) = cleanup(&recovered, &codebook).unwrap();
        assert_eq!(best_label, "kind_3",
            "expected kind_3 but got {best_label} (sim={best_sim:.4})");
    }

    #[test]
    fn cleanup_all_roles_from_bundle() {
        let mut codebook = Codebook::new(42);
        let mut rng = Rng::new(99);

        let labels: Vec<String> = (0..20).map(|i| format!("node_{i}")).collect();
        for l in &labels {
            codebook.get_or_create(l);
        }

        let roles: Vec<HrrVec> = (0..5).map(|_| HrrVec::random(&mut rng)).collect();
        let filler_indices = [2, 7, 11, 15, 18];

        let pairs: Vec<HrrVec> = roles.iter().zip(&filler_indices)
            .map(|(role, &idx)| role.bind(&codebook.get_or_create(&labels[idx])))
            .collect();
        let bundled = bundle(&pairs);

        let mut correct = 0;
        for (i, role) in roles.iter().enumerate() {
            let recovered = bundled.unbind(role);
            if let Some((label, _)) = cleanup(&recovered, &codebook) {
                if label == labels[filler_indices[i]] {
                    correct += 1;
                }
            }
        }

        assert!(correct >= 3,
            "recovered {correct}/5 fillers — expected >= 3 for dim={DEFAULT_DIM}");
    }

    #[test]
    fn permute_is_invertible() {
        let mut rng = Rng::new(42);
        let v = HrrVec::random(&mut rng);
        let shifted = v.permute(7);
        let back = shifted.permute(DEFAULT_DIM - 7);
        let sim = v.cosine_similarity(&back);
        assert!((sim - 1.0).abs() < 1e-10, "sim={sim}");
    }

    #[test]
    fn permute_is_orthogonal() {
        let mut rng = Rng::new(42);
        let v = HrrVec::random(&mut rng);
        let p = v.permute(7);
        let sim = v.cosine_similarity(&p);
        assert!(sim.abs() < 0.15, "sim={sim}");
    }

    #[test]
    fn bundle_capacity_curve() {
        let mut rng = Rng::new(42);
        let mut vecs: Vec<HrrVec> = Vec::new();

        for n in [3, 5, 10, 20, 50] {
            while vecs.len() < n {
                vecs.push(HrrVec::random(&mut rng));
            }
            let bundled = bundle(&vecs[..n]);
            let avg_sim: f64 = vecs[..n].iter()
                .map(|v| bundled.cosine_similarity(v))
                .sum::<f64>() / n as f64;
            let expected = 1.0 / (n as f64).sqrt();
            assert!(avg_sim > expected * 0.3,
                "n={n}: avg_sim={avg_sim:.4}, expected ~{expected:.4}");
        }
    }
}
