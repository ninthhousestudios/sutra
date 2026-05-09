// Analogical reasoning spike.
//
// Tests whether HRR vector arithmetic produces meaningful
// "king - man + woman = queen" style analogies for code constructs.
//
// Key question: can we derive transformations from examples and
// apply them to new code?

#[cfg(test)]
mod tests {
    use crate::hrr::{HrrVec, Rng, Codebook, cleanup};

    fn encode_fn(roles: &[(&str, &HrrVec)], cb: &mut Codebook) -> HrrVec {
        let mut sum = HrrVec::zero();
        for (filler_name, role) in roles {
            let filler = cb.get_or_create(filler_name);
            sum = sum.add(&role.bind(&filler));
        }
        sum
    }

    #[test]
    fn clean_two_role_analogy() {
        // fn_a: type=Result, body=simple
        // fn_b: type=Option, body=simple
        // fn_c: type=Option, body=complex
        // fn_a - fn_b + fn_c ≈ fn_d(type=Result, body=complex)
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let type_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let fn_a = encode_fn(&[("Result", &type_role), ("simple", &body_role)], &mut cb);
        let fn_b = encode_fn(&[("Option", &type_role), ("simple", &body_role)], &mut cb);
        let fn_c = encode_fn(&[("Option", &type_role), ("complex", &body_role)], &mut cb);
        let fn_d = encode_fn(&[("Result", &type_role), ("complex", &body_role)], &mut cb);

        let analogy = fn_a.sub(&fn_b).add(&fn_c);
        let sim = analogy.cosine_similarity(&fn_d);
        assert!(sim > 0.95, "clean 2-role analogy sim={sim:.4}");
    }

    #[test]
    fn six_roles_one_differs() {
        // 6 roles, only return type differs between a/b.
        // Shared components cancel in subtraction.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let roles: Vec<HrrVec> = (0..6).map(|_| HrrVec::random(&mut rng)).collect();

        let fn_a = encode_fn(&[
            ("Result", &roles[0]),
            ("loop_body", &roles[1]),
            ("two_args", &roles[2]),
            ("pub", &roles[3]),
            ("static", &roles[4]),
            ("propagate", &roles[5]),
        ], &mut cb);

        let fn_b = encode_fn(&[
            ("Option", &roles[0]),
            ("loop_body", &roles[1]),
            ("two_args", &roles[2]),
            ("pub", &roles[3]),
            ("static", &roles[4]),
            ("propagate", &roles[5]),
        ], &mut cb);

        let fn_c = encode_fn(&[
            ("Option", &roles[0]),
            ("branch_body", &roles[1]),
            ("one_arg", &roles[2]),
            ("crate", &roles[3]),
            ("bounded", &roles[4]),
            ("unwrap", &roles[5]),
        ], &mut cb);

        let fn_d = encode_fn(&[
            ("Result", &roles[0]),
            ("branch_body", &roles[1]),
            ("one_arg", &roles[2]),
            ("crate", &roles[3]),
            ("bounded", &roles[4]),
            ("unwrap", &roles[5]),
        ], &mut cb);

        let analogy = fn_a.sub(&fn_b).add(&fn_c);
        let sim = analogy.cosine_similarity(&fn_d);
        assert!(sim > 0.95, "6-role analogy sim={sim:.4}");

        let recovered = analogy.unbind(&roles[0]);
        let sim_result = recovered.cosine_similarity(&cb.get_or_create("Result"));
        let sim_option = recovered.cosine_similarity(&cb.get_or_create("Option"));
        assert!(sim_result > sim_option,
            "result={sim_result:.4} vs option={sim_option:.4}");
    }

    #[test]
    fn analogy_with_noise() {
        // Add Gaussian noise simulating imprecision of real tree encodings.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let type_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);
        let param_role = HrrVec::random(&mut rng);

        let add_noise = |v: &HrrVec, rng: &mut Rng, scale: f64| -> HrrVec {
            let noise = HrrVec::random(rng).scale(scale);
            v.add(&noise)
        };

        let fn_a = add_noise(
            &encode_fn(&[("Result", &type_role), ("simple", &body_role),
                         ("two_args", &param_role)], &mut cb),
            &mut rng, 0.3);
        let fn_b = add_noise(
            &encode_fn(&[("Option", &type_role), ("simple", &body_role),
                         ("two_args", &param_role)], &mut cb),
            &mut rng, 0.3);
        let fn_c = add_noise(
            &encode_fn(&[("Option", &type_role), ("complex", &body_role),
                         ("one_arg", &param_role)], &mut cb),
            &mut rng, 0.3);
        let fn_d = encode_fn(&[("Result", &type_role), ("complex", &body_role),
                                ("one_arg", &param_role)], &mut cb);

        let analogy = fn_a.sub(&fn_b).add(&fn_c);
        let sim = analogy.cosine_similarity(&fn_d);
        assert!(sim > 0.4, "noisy analogy sim={sim:.4}");
    }

    #[test]
    fn cleanup_all_roles_after_analogy() {
        // Full pipeline: analogy → unbind each role → codebook cleanup.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let roles: Vec<HrrVec> = (0..4).map(|_| HrrVec::random(&mut rng)).collect();

        for name in ["Result", "Option", "bool", "void",
                      "simple", "complex", "recursive",
                      "zero_arg", "one_arg", "two_arg",
                      "pub", "crate", "private"] {
            cb.get_or_create(name);
        }

        let fn_a = encode_fn(&[
            ("Result", &roles[0]), ("simple", &roles[1]),
            ("one_arg", &roles[2]), ("pub", &roles[3]),
        ], &mut cb);
        let fn_b = encode_fn(&[
            ("Option", &roles[0]), ("simple", &roles[1]),
            ("one_arg", &roles[2]), ("pub", &roles[3]),
        ], &mut cb);
        let fn_c = encode_fn(&[
            ("Option", &roles[0]), ("recursive", &roles[1]),
            ("two_arg", &roles[2]), ("private", &roles[3]),
        ], &mut cb);

        let analogy = fn_a.sub(&fn_b).add(&fn_c);

        let (ret, _) = cleanup(&analogy.unbind(&roles[0]), &cb).unwrap();
        let (body, _) = cleanup(&analogy.unbind(&roles[1]), &cb).unwrap();
        let (param, _) = cleanup(&analogy.unbind(&roles[2]), &cb).unwrap();
        let (vis, _) = cleanup(&analogy.unbind(&roles[3]), &cb).unwrap();

        assert_eq!(ret, "Result", "return type");
        assert_eq!(body, "recursive", "body shape");
        assert_eq!(param, "two_arg", "param count");
        assert_eq!(vis, "private", "visibility");
    }

    #[test]
    fn extract_and_reapply_transformation() {
        // Learn "make async" from one example, apply to two others.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let modifier_role = HrrVec::random(&mut rng);
        let return_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let sync_1 = encode_fn(&[
            ("sync", &modifier_role), ("Vec", &return_role), ("simple", &body_role),
        ], &mut cb);
        let async_1 = encode_fn(&[
            ("async", &modifier_role), ("Vec", &return_role), ("simple", &body_role),
        ], &mut cb);

        let transform = async_1.sub(&sync_1);

        let sync_2 = encode_fn(&[
            ("sync", &modifier_role), ("HashMap", &return_role), ("complex", &body_role),
        ], &mut cb);
        let expected_2 = encode_fn(&[
            ("async", &modifier_role), ("HashMap", &return_role), ("complex", &body_role),
        ], &mut cb);

        let predicted_2 = sync_2.add(&transform);
        let sim_2 = predicted_2.cosine_similarity(&expected_2);
        assert!(sim_2 > 0.9, "transform apply 1: sim={sim_2:.4}");

        let sync_3 = encode_fn(&[
            ("sync", &modifier_role), ("String", &return_role), ("recursive", &body_role),
        ], &mut cb);
        let expected_3 = encode_fn(&[
            ("async", &modifier_role), ("String", &return_role), ("recursive", &body_role),
        ], &mut cb);

        let predicted_3 = sync_3.add(&transform);
        let sim_3 = predicted_3.cosine_similarity(&expected_3);
        assert!(sim_3 > 0.9, "transform apply 2: sim={sim_3:.4}");
    }

    #[test]
    fn nearest_neighbor_after_transform() {
        // Given a corpus of encoded functions, does the transformed vector
        // land nearest to the correct target?
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let r0 = HrrVec::random(&mut rng);
        let r1 = HrrVec::random(&mut rng);
        let r2 = HrrVec::random(&mut rng);

        let sync_a = encode_fn(&[("sync", &r0), ("Result", &r1), ("simple", &r2)], &mut cb);
        let async_a = encode_fn(&[("async", &r0), ("Result", &r1), ("simple", &r2)], &mut cb);
        let transform = async_a.sub(&sync_a);

        let corpus: Vec<(&str, HrrVec)> = vec![
            ("async_option_complex",
             encode_fn(&[("async", &r0), ("Option", &r1), ("complex", &r2)], &mut cb)),
            ("sync_option_complex",
             encode_fn(&[("sync", &r0), ("Option", &r1), ("complex", &r2)], &mut cb)),
            ("async_result_complex",
             encode_fn(&[("async", &r0), ("Result", &r1), ("complex", &r2)], &mut cb)),
            ("sync_result_complex",
             encode_fn(&[("sync", &r0), ("Result", &r1), ("complex", &r2)], &mut cb)),
        ];

        let query = encode_fn(&[("sync", &r0), ("Result", &r1), ("complex", &r2)], &mut cb);
        let transformed = query.add(&transform);

        let (best_name, _) = corpus.iter()
            .map(|(name, vec)| (*name, transformed.cosine_similarity(vec)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();

        assert_eq!(best_name, "async_result_complex",
            "expected async_result_complex, got {best_name}");
    }
}
