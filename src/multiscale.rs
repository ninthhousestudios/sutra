// Multi-scale composition spike.
//
// Tests hierarchical encoding: statement → block → function → module.
// Key question: can you encode at multiple scales and still retrieve
// at any level? Where does the signal attenuate past usefulness?

#[cfg(test)]
mod tests {
    use crate::hrr::{HrrVec, Rng, Codebook, bundle, cleanup};

    #[test]
    fn statement_retrieval_from_block() {
        // Bundle 5 statement vectors. Can we detect membership?
        let mut cb = Codebook::new(99);

        let members: Vec<HrrVec> = ["assign", "call_db", "branch", "call_log", "return"]
            .iter()
            .map(|s| cb.get_or_create(s))
            .collect();

        let block = bundle(&members);

        let non_member = cb.get_or_create("loop");

        let min_member_sim = members.iter()
            .map(|m| block.cosine_similarity(m))
            .fold(f64::INFINITY, f64::min);
        let non_member_sim = block.cosine_similarity(&non_member).abs();

        assert!(min_member_sim > non_member_sim,
            "weakest member sim={min_member_sim:.4} should > non-member sim={non_member_sim:.4}");
    }

    #[test]
    fn function_decomposition_via_unbind() {
        // function = bind(sig_role, name) + bind(body_role, block)
        // Unbinding sig_role should recover name.
        // Unbinding body_role should recover block.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let sig_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let name = cb.get_or_create("process_data");
        let body_stmts: Vec<HrrVec> = ["validate", "transform", "persist", "return"]
            .iter()
            .map(|s| cb.get_or_create(s))
            .collect();
        let body = bundle(&body_stmts);

        let func = sig_role.bind(&name).add(&body_role.bind(&body));

        // Recover name
        let recovered_name = func.unbind(&sig_role);
        let (label, _) = cleanup(&recovered_name, &cb).unwrap();
        assert_eq!(label, "process_data", "recovered name");

        // Recover body and check it's similar to the original block
        let recovered_body = func.unbind(&body_role);
        let body_sim = recovered_body.cosine_similarity(&body);
        assert!(body_sim > 0.3, "body recovery sim={body_sim:.4}");
    }

    #[test]
    fn similar_body_different_name() {
        // Two functions with same body but different names should be
        // more similar to each other than to a function with different body.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let sig_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let body_a_stmts: Vec<HrrVec> = ["validate", "parse", "return"]
            .iter().map(|s| cb.get_or_create(s)).collect();
        let body_a = bundle(&body_a_stmts);

        let body_b_stmts: Vec<HrrVec> = ["alloc", "write", "free"]
            .iter().map(|s| cb.get_or_create(s)).collect();
        let body_b = bundle(&body_b_stmts);

        let fn1 = sig_role.bind(&cb.get_or_create("parse_input"))
            .add(&body_role.bind(&body_a));
        let fn2 = sig_role.bind(&cb.get_or_create("parse_config"))
            .add(&body_role.bind(&body_a));
        let fn3 = sig_role.bind(&cb.get_or_create("alloc_buffer"))
            .add(&body_role.bind(&body_b));

        let sim_same_body = fn1.cosine_similarity(&fn2);
        let sim_diff_body = fn1.cosine_similarity(&fn3);

        assert!(sim_same_body > sim_diff_body,
            "same_body={sim_same_body:.4} should > diff_body={sim_diff_body:.4}");
    }

    #[test]
    fn module_contains_functions() {
        // module = bundle(fn1, fn2, ..., fn5)
        // Each member function should be more similar to the module
        // than a non-member function.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let sig_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let fn_specs: Vec<(&str, &[&str])> = vec![
            ("parse", &["tokenize", "build_ast", "return"]),
            ("validate", &["check_types", "check_bounds", "return"]),
            ("transform", &["map", "filter", "collect"]),
            ("serialize", &["format", "write_buf", "flush"]),
            ("handle_error", &["match_err", "log", "recover"]),
        ];

        let functions: Vec<HrrVec> = fn_specs.iter().map(|(name, stmts)| {
            let body_vecs: Vec<HrrVec> = stmts.iter()
                .map(|s| cb.get_or_create(s)).collect();
            let body = bundle(&body_vecs);
            sig_role.bind(&cb.get_or_create(name))
                .add(&body_role.bind(&body))
        }).collect();

        let module = bundle(&functions);

        let outsider_body: Vec<HrrVec> = ["socket_open", "read_stream", "close"]
            .iter().map(|s| cb.get_or_create(s)).collect();
        let outsider = sig_role.bind(&cb.get_or_create("network_io"))
            .add(&body_role.bind(&bundle(&outsider_body)));

        let member_sims: Vec<f64> = functions.iter()
            .map(|f| module.cosine_similarity(f))
            .collect();
        let outsider_sim = module.cosine_similarity(&outsider);

        let min_member = member_sims.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(min_member > outsider_sim,
            "weakest member sim={min_member:.4} should > outsider sim={outsider_sim:.4}");
    }

    #[test]
    fn two_hop_retrieval() {
        // module → unbind body_role from best-matching fn → check statement membership
        // This is the multi-scale retrieval pipeline.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let sig_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let body1_stmts: Vec<HrrVec> = ["call_api", "parse_json", "return"]
            .iter().map(|s| cb.get_or_create(s)).collect();
        let body1 = bundle(&body1_stmts);
        let fn1 = sig_role.bind(&cb.get_or_create("fetch_data"))
            .add(&body_role.bind(&body1));

        let body2_stmts: Vec<HrrVec> = ["validate", "transform", "persist"]
            .iter().map(|s| cb.get_or_create(s)).collect();
        let body2 = bundle(&body2_stmts);
        let fn2 = sig_role.bind(&cb.get_or_create("process"))
            .add(&body_role.bind(&body2));

        let module = bundle(&[fn1.clone(), fn2.clone()]);

        // Hop 1: which function is fn1 most similar to?
        let sim_fn1 = module.cosine_similarity(&fn1);
        let sim_fn2 = module.cosine_similarity(&fn2);
        assert!(sim_fn1 > 0.0 && sim_fn2 > 0.0, "both should be detectable");

        // Hop 2: unbind body_role from fn1 to recover its body
        let recovered_body = fn1.unbind(&body_role);

        // Check that "call_api" is detectable in the recovered body
        let call_api = cb.get_or_create("call_api");
        let validate = cb.get_or_create("validate");

        let sim_present = recovered_body.cosine_similarity(&call_api);
        let sim_absent = recovered_body.cosine_similarity(&validate);

        assert!(sim_present.abs() > sim_absent.abs(),
            "present={sim_present:.4} should > absent={sim_absent:.4} in recovered body");
    }

    #[test]
    fn capacity_across_scales() {
        // How many functions can a module bundle before retrieval degrades?
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let sig_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let mut functions = Vec::new();
        let sizes = [3, 5, 10, 20, 50];

        for &n in &sizes {
            while functions.len() < n {
                let idx = functions.len();
                let body_stmts: Vec<HrrVec> = (0..3)
                    .map(|j| cb.get_or_create(&format!("op_{idx}_{j}")))
                    .collect();
                let body = bundle(&body_stmts);
                let func = sig_role.bind(&cb.get_or_create(&format!("fn_{idx}")))
                    .add(&body_role.bind(&body));
                functions.push(func);
            }

            let module = bundle(&functions[..n]);
            let avg_sim: f64 = functions[..n].iter()
                .map(|f| module.cosine_similarity(f))
                .sum::<f64>() / n as f64;

            let expected = 1.0 / (n as f64).sqrt();
            assert!(avg_sim > expected * 0.3,
                "n={n}: avg_sim={avg_sim:.4}, expected ~{expected:.4}");
        }
    }

    #[test]
    fn binding_prevents_cross_scale_leakage() {
        // A raw statement vector should NOT match a function that contains it,
        // because bind(body_role, ...) makes the body orthogonal to its components.
        // This is correct behavior: you need the role key to access the content.
        let mut rng = Rng::new(42);
        let mut cb = Codebook::new(99);

        let sig_role = HrrVec::random(&mut rng);
        let body_role = HrrVec::random(&mut rng);

        let stmt = cb.get_or_create("dangerous_op");
        let body = bundle(&[
            stmt.clone(),
            cb.get_or_create("setup"),
            cb.get_or_create("cleanup"),
        ]);

        let func = sig_role.bind(&cb.get_or_create("do_thing"))
            .add(&body_role.bind(&body));

        // Direct similarity between raw statement and function should be low
        let direct_sim = func.cosine_similarity(&stmt);

        // But unbinding body_role then checking should find it
        let recovered_body = func.unbind(&body_role);
        let via_unbind_sim = recovered_body.cosine_similarity(&stmt);

        assert!(via_unbind_sim.abs() > direct_sim.abs() * 2.0,
            "via_unbind={via_unbind_sim:.4} should >> direct={direct_sim:.4}");
    }
}
