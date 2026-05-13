use super::bitset::BitSet;

pub struct FormalContext {
    #[allow(dead_code)] // used by downstream violation reporting
    pub(crate) object_names: Vec<String>,
    pub(crate) attribute_names: Vec<String>,
    pub(crate) object_attrs: Vec<BitSet>,
    pub(crate) attr_objects: Vec<BitSet>,
    n_objects: usize,
    n_attrs: usize,
}

impl FormalContext {
    pub fn new(
        object_names: Vec<String>,
        attribute_names: Vec<String>,
        relations: &[(usize, usize)],
    ) -> Self {
        let n_objects = object_names.len();
        let n_attrs = attribute_names.len();
        let mut object_attrs: Vec<BitSet> = (0..n_objects).map(|_| BitSet::new(n_attrs)).collect();
        let mut attr_objects: Vec<BitSet> = (0..n_attrs).map(|_| BitSet::new(n_objects)).collect();

        for &(g, m) in relations {
            object_attrs[g].set(m);
            attr_objects[m].set(g);
        }

        Self {
            object_names,
            attribute_names,
            object_attrs,
            attr_objects,
            n_objects,
            n_attrs,
        }
    }

    pub fn n_objects(&self) -> usize {
        self.n_objects
    }

    pub fn n_attrs(&self) -> usize {
        self.n_attrs
    }

    pub fn object_prime(&self, objects: &BitSet) -> BitSet {
        if objects.is_empty() {
            return BitSet::from_indices(self.n_attrs, 0..self.n_attrs);
        }
        let mut result = BitSet::from_indices(self.n_attrs, 0..self.n_attrs);
        for g in objects.iter() {
            result = result.intersect(&self.object_attrs[g]);
        }
        result
    }

    pub fn attr_prime(&self, attrs: &BitSet) -> BitSet {
        if attrs.is_empty() {
            return BitSet::from_indices(self.n_objects, 0..self.n_objects);
        }
        let mut result = BitSet::from_indices(self.n_objects, 0..self.n_objects);
        for m in attrs.iter() {
            result = result.intersect(&self.attr_objects[m]);
        }
        result
    }

    pub fn closure(&self, attrs: &BitSet) -> BitSet {
        self.object_prime(&self.attr_prime(attrs))
    }

    pub fn next_closure(&self, current: &BitSet) -> Option<BitSet> {
        for i in (0..self.n_attrs).rev() {
            if current.contains(i) {
                continue;
            }
            let mut candidate = BitSet::new(self.n_attrs);
            for j in 0..i {
                if current.contains(j) {
                    candidate.set(j);
                }
            }
            candidate.set(i);

            let closed = self.closure(&candidate);

            let mut valid = true;
            for j in 0..i {
                if closed.contains(j) != candidate.contains(j) {
                    valid = false;
                    break;
                }
            }
            if valid {
                return Some(closed);
            }
        }
        None
    }

    pub fn all_concepts(&self) -> Vec<(BitSet, BitSet)> {
        let mut concepts = Vec::new();
        let mut current = self.closure(&BitSet::new(self.n_attrs));
        let extent = self.attr_prime(&current);
        concepts.push((extent, current.clone()));

        while let Some(next) = self.next_closure(&current) {
            let extent = self.attr_prime(&next);
            concepts.push((extent, next.clone()));
            current = next;
        }
        concepts
    }

    pub fn approximate_implications(
        &self,
        min_support: usize,
        min_confidence: f64,
    ) -> Vec<Implication> {
        let mut implications = Vec::new();

        for a in 0..self.n_attrs {
            let support_a = self.attr_objects[a].count();
            if support_a < min_support {
                continue;
            }

            for b in 0..self.n_attrs {
                if a == b {
                    continue;
                }
                let both = self.attr_objects[a].intersect(&self.attr_objects[b]);
                let support_ab = both.count();
                let confidence = support_ab as f64 / support_a as f64;

                if confidence >= min_confidence && confidence < 1.0 && support_ab >= min_support {
                    implications.push(Implication {
                        antecedent: vec![self.attribute_names[a].clone()],
                        consequent: vec![self.attribute_names[b].clone()],
                        support: support_ab,
                        confidence,
                    });
                }
            }
        }

        implications.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap()
                .then(b.support.cmp(&a.support))
        });
        implications
    }
}

#[derive(Debug, Clone)]
pub struct Implication {
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub support: usize,
    pub confidence: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_context() -> FormalContext {
        // Objects: a, b, c
        // Attributes: x, y, z
        // Relations: a-x, a-y, b-x, b-z, c-y, c-z
        FormalContext::new(
            vec!["a".into(), "b".into(), "c".into()],
            vec!["x".into(), "y".into(), "z".into()],
            &[(0, 0), (0, 1), (1, 0), (1, 2), (2, 1), (2, 2)],
        )
    }

    #[test]
    fn closure_of_empty_returns_attributes_common_to_all() {
        let ctx = small_context();
        let closed = ctx.closure(&BitSet::new(3));
        // All objects have... no single attribute in common (a has x,y; b has x,z; c has y,z)
        assert_eq!(closed.count(), 0);
    }

    #[test]
    fn closure_of_single_attribute() {
        let ctx = small_context();
        // closure({x}) = attributes common to all objects having x
        // Objects with x: a (x,y), b (x,z) → common attributes: {x}
        let mut x = BitSet::new(3);
        x.set(0); // x
        let closed = ctx.closure(&x);
        assert!(closed.contains(0)); // x
        assert!(!closed.contains(1)); // not y
        assert!(!closed.contains(2)); // not z
    }

    #[test]
    fn closure_is_idempotent() {
        let ctx = small_context();
        let mut xy = BitSet::new(3);
        xy.set(0);
        xy.set(1);
        let closed1 = ctx.closure(&xy);
        let closed2 = ctx.closure(&closed1);
        assert_eq!(closed1, closed2);
    }

    #[test]
    fn attr_prime_gives_objects_with_all_attributes() {
        let ctx = small_context();
        let mut xz = BitSet::new(3);
        xz.set(0); // x
        xz.set(2); // z
        let extent = ctx.attr_prime(&xz);
        // Only b has both x and z
        assert_eq!(extent.count(), 1);
        assert!(extent.contains(1));
    }

    #[test]
    fn next_closure_enumerates_all_concepts() {
        let ctx = small_context();
        let concepts = ctx.all_concepts();
        // 3 objects each with 2 of 3 attributes, every pair shares exactly 1
        // Expected concepts: ∅→{a,b,c}, {x}→{a,b}, {y}→{a,c}, {z}→{b,c},
        //   {x,y}→{a}, {x,z}→{b}, {y,z}→{c}, {x,y,z}→∅
        assert_eq!(concepts.len(), 8);
    }

    fn convention_context() -> FormalContext {
        // 10 functions: 9 have both "kind:function" and "has_sig", 1 has "kind:function" only
        // This gives an approximate implication: kind:function → has_sig (conf=0.9)
        let mut objects = Vec::new();
        let mut relations = Vec::new();
        for i in 0..10 {
            objects.push(format!("fn_{i}"));
            relations.push((i, 0)); // kind:function
            if i < 9 {
                relations.push((i, 1)); // has_sig
            }
        }
        // Add 5 structs with "kind:struct" and "naming:CamelCase"
        for i in 0..5 {
            let idx = 10 + i;
            objects.push(format!("Struct{i}"));
            relations.push((idx, 2)); // kind:struct
            relations.push((idx, 3)); // naming:CamelCase
        }
        FormalContext::new(
            objects,
            vec![
                "kind:function".into(),
                "has_sig".into(),
                "kind:struct".into(),
                "naming:CamelCase".into(),
            ],
            &relations,
        )
    }

    #[test]
    fn approximate_implications_at_threshold() {
        let ctx = convention_context();
        let impls = ctx.approximate_implications(3, 0.9);
        // kind:function → has_sig should appear (9/10 = 0.9)
        let found = impls
            .iter()
            .any(|i| i.antecedent == vec!["kind:function"] && i.consequent == vec!["has_sig"]);
        assert!(found, "expected kind:function → has_sig, got: {impls:?}");
    }

    #[test]
    fn approximate_implications_exclude_exact() {
        let ctx = convention_context();
        let impls = ctx.approximate_implications(3, 0.9);
        // kind:struct → naming:CamelCase is exact (1.0), should NOT appear
        let found = impls.iter().any(|i| {
            i.antecedent == vec!["kind:struct"] && i.consequent == vec!["naming:CamelCase"]
        });
        assert!(!found, "exact implications should be excluded");
    }

    #[test]
    fn concepts_include_top_and_bottom() {
        let ctx = small_context();
        let concepts = ctx.all_concepts();
        let top = concepts.first().unwrap();
        assert_eq!(top.0.count(), 3); // all objects
        assert_eq!(top.1.count(), 0); // no common attributes
        let bottom = concepts.last().unwrap();
        assert_eq!(bottom.0.count(), 0); // no objects
        assert_eq!(bottom.1.count(), 3); // all attributes
    }
}
