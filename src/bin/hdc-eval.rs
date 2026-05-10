use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use sutra::hdc::{self, Codebook as MapCodebook, HdcVec, IdentMode, Rng as MapRng};
use sutra::hrr::{self, Codebook as HrrCodebook, HrrVec, Rng as HrrRng};

struct EncodedFn {
    file: String,
    name: String,
    traits: Vec<&'static str>,
    // Binary MAP vectors
    map_strip: HdcVec,
    map_embed: HdcVec,
    map_bigram: Option<HdcVec>,
    // HRR vectors
    hrr_strip: HrrVec,
    hrr_embed: HrrVec,
    hrr_bigram: Option<HrrVec>,
    // Shared
    ops: Vec<hdc::Op>,
}

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    C,
}

#[derive(Clone, Copy)]
enum Verdict {
    Works,
    NeedsWork,
    NotViable,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Works => write!(f, "WORKS"),
            Verdict::NeedsWork => write!(f, "NEEDS WORK"),
            Verdict::NotViable => write!(f, "NOT VIABLE"),
        }
    }
}

struct TestResult {
    name: &'static str,
    map_summary: String,
    hrr_summary: String,
    verdict_map: Verdict,
    verdict_hrr: Verdict,
    recommendation: &'static str,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or("src");

    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");

    println!("=== HDC Practical Evaluation: MAP vs HRR ===\n");
    println!("Root: {root}");
    println!("Files: {} ({} .rs, {} .c)\n",
        rs_files.len() + c_files.len(), rs_files.len(), c_files.len());

    // --- Encode with both representations ---
    let t_map = Instant::now();
    let mut map_cb_embed = MapCodebook::new(42);
    let mut map_cb_strip = MapCodebook::new(42);
    let mut map_cb_seq = MapCodebook::new(99);
    let mut map_rng = MapRng::new(123);

    let mut hrr_cb_embed = HrrCodebook::new(42);
    let mut hrr_cb_strip = HrrCodebook::new(42);
    let mut hrr_cb_seq = HrrCodebook::new(99);
    let mut hrr_rng = HrrRng::new(123);

    let mut rs_parser = tree_sitter::Parser::new();
    rs_parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
    let mut c_parser = tree_sitter::Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into()).unwrap();

    let mut functions: Vec<EncodedFn> = Vec::new();

    for (files, lang, parser) in [
        (&rs_files, Lang::Rust, &mut rs_parser),
        (&c_files, Lang::C, &mut c_parser),
    ] {
        for path in files {
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let tree = match parser.parse(&source, None) {
                Some(t) => t,
                None => continue,
            };
            for (name, node_id) in extract_function_nodes(&tree, source.as_bytes(), lang) {
                let node = find_node_by_id(&tree.root_node(), node_id).unwrap();
                let traits = classify_node(&node, source.as_bytes(), lang);
                let ops = hdc::extract_ops(&node, source.as_bytes());

                let map_strip = hdc::encode(&node, source.as_bytes(), &mut map_cb_strip, &mut map_rng, 5, IdentMode::Strip);
                let map_embed = hdc::encode(&node, source.as_bytes(), &mut map_cb_embed, &mut map_rng, 5, IdentMode::Embed);
                let map_bigram = hdc::encode_bigrams(&ops, &mut map_cb_seq, &mut map_rng);

                let hrr_strip = encode_hrr(&node, source.as_bytes(), &mut hrr_cb_strip, &mut hrr_rng, 5, false);
                let hrr_embed = encode_hrr(&node, source.as_bytes(), &mut hrr_cb_embed, &mut hrr_rng, 5, true);
                let hrr_bigram = encode_bigrams_hrr(&ops, &mut hrr_cb_seq, &mut hrr_rng);

                functions.push(EncodedFn {
                    file: path.display().to_string(), name, traits, ops,
                    map_strip, map_embed, map_bigram,
                    hrr_strip, hrr_embed, hrr_bigram,
                });
            }
        }
    }

    let map_elapsed = t_map.elapsed();

    if functions.is_empty() {
        println!("No functions found. Check the path.");
        return;
    }

    // Time encoding separately for the performance test
    let (map_time_us, hrr_time_us) = bench_encoding(root, &rs_files, &c_files);

    println!("Encoded {} functions in {:.1}ms (combined)\n", functions.len(), map_elapsed.as_secs_f64() * 1000.0);

    // --- Run all 5 tests ---
    let mut results = Vec::new();

    results.push(test_structural_search(&functions));
    results.push(test_decomposition(&functions, &mut map_rng, &mut hrr_rng));
    results.push(test_transform_search(&functions));
    results.push(test_cross_file_diff(&functions));
    results.push(test_performance(
        functions.len(), map_time_us, hrr_time_us,
        map_cb_strip.len(), hrr_cb_strip.len(),
    ));

    // --- Summary table ---
    println!("\n{}", "=".repeat(70));
    println!("=== SUMMARY ===\n");
    println!("{:<22} {:<16} {:<16} {:<12}", "Test", "MAP", "HRR", "Recommend");
    println!("{:-<22} {:-<16} {:-<16} {:-<12}", "", "", "", "");
    for r in &results {
        println!("{:<22} {:<16} {:<16} {}", r.name, r.map_summary, r.hrr_summary, r.recommendation);
    }
    println!();
    println!("Verdict key: WORKS = ship-ready, NEEDS WORK = promising but gaps, NOT VIABLE = doesn't help");
    println!();

    // Design recommendation
    let map_works = results.iter().filter(|r| matches!(r.verdict_map, Verdict::Works)).count();
    let hrr_works = results.iter().filter(|r| matches!(r.verdict_hrr, Verdict::Works)).count();
    let hrr_exclusive = results.iter()
        .filter(|r| matches!(r.verdict_hrr, Verdict::Works) && !matches!(r.verdict_map, Verdict::Works))
        .count();

    println!("=== DESIGN RECOMMENDATION ===\n");
    println!("MAP passes: {map_works}/5, HRR passes: {hrr_works}/5, HRR-exclusive: {hrr_exclusive}");
    if hrr_exclusive == 0 {
        println!("→ Binary MAP only. HRR adds no capabilities that MAP can't cover.");
    } else if map_works == 0 {
        println!("→ HRR only. Binary MAP doesn't pass any tests.");
    } else {
        println!("→ Hybrid: binary MAP for indexing/similarity, HRR for decomposition/analogy.");
    }
}

// ===== Test 1: Structural search =====

fn test_structural_search(fns: &[EncodedFn]) -> TestResult {
    println!("=== Test 1: Structural search ===");
    println!("\"Find functions shaped like X\" via prototype similarity\n");

    let traits_to_test = [
        "error-handling", "loop", "match", "conditional", "unsafe",
        "closure", "early-return",
    ];

    let mut map_avg_lift = 0.0;
    let mut hrr_avg_lift = 0.0;
    let mut count = 0;

    println!("  {:>16} {:>5} {:>8} {:>8} {:>8} {:>8} {:>10}",
        "trait", "n", "MAP P@1", "HRR P@1", "MAP P@5", "HRR P@5", "base_rate");

    for tr in &traits_to_test {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        if group.len() < 5 { continue; }

        let base_rate = group.len() as f64 / fns.len() as f64;
        let sample = group.len().min(100);

        let mut map_p1 = 0;
        let mut hrr_p1 = 0;
        let mut map_p5 = 0;
        let mut hrr_p5 = 0;

        for &i in group.iter().take(sample) {
            let map_nn = k_nearest_map(i, fns, |f| &f.map_strip, 5);
            let hrr_nn = k_nearest_hrr(i, fns, |f| &f.hrr_strip, 5);

            if fns[map_nn[0]].traits.contains(tr) { map_p1 += 1; }
            if fns[hrr_nn[0]].traits.contains(tr) { hrr_p1 += 1; }
            if map_nn.iter().any(|&j| fns[j].traits.contains(tr)) { map_p5 += 1; }
            if hrr_nn.iter().any(|&j| fns[j].traits.contains(tr)) { hrr_p5 += 1; }
        }

        let map_p1_pct = pct(map_p1, sample);
        let hrr_p1_pct = pct(hrr_p1, sample);
        let map_p5_pct = pct(map_p5, sample);
        let hrr_p5_pct = pct(hrr_p5, sample);

        println!("  {:>16} {:>5} {:>7.0}% {:>7.0}% {:>7.0}% {:>7.0}% {:>9.0}%",
            tr, group.len(), map_p1_pct, hrr_p1_pct, map_p5_pct, hrr_p5_pct,
            base_rate * 100.0);

        if base_rate > 0.0 {
            map_avg_lift += map_p1_pct / (base_rate * 100.0);
            hrr_avg_lift += hrr_p1_pct / (base_rate * 100.0);
            count += 1;
        }
    }

    if count > 0 {
        map_avg_lift /= count as f64;
        hrr_avg_lift /= count as f64;
    }

    println!("\n  Avg P@1 lift over base rate: MAP {map_avg_lift:.1}x, HRR {hrr_avg_lift:.1}x\n");

    let map_verdict = if map_avg_lift >= 1.5 { Verdict::Works } else if map_avg_lift >= 1.2 { Verdict::NeedsWork } else { Verdict::NotViable };
    let hrr_verdict = if hrr_avg_lift >= 1.5 { Verdict::Works } else if hrr_avg_lift >= 1.2 { Verdict::NeedsWork } else { Verdict::NotViable };

    TestResult {
        name: "Structural search",
        map_summary: format!("{:.1}x lift", map_avg_lift),
        hrr_summary: format!("{:.1}x lift", hrr_avg_lift),
        verdict_map: map_verdict,
        verdict_hrr: hrr_verdict,
        recommendation: if (map_avg_lift - hrr_avg_lift).abs() < 0.3 { "Equivalent" } else if map_avg_lift > hrr_avg_lift { "MAP" } else { "HRR" },
    }
}

// ===== Test 2: Decomposition queries =====

fn test_decomposition(
    fns: &[EncodedFn],
    map_rng: &mut MapRng,
    _hrr_rng: &mut HrrRng,
) -> TestResult {
    println!("=== Test 2: Decomposition — \"what's this function's X strategy?\" ===");
    println!("Unbind a trait prototype from a function, cleanup against codebook\n");

    let test_traits = ["error-handling", "loop", "match", "unsafe"];

    // Build all trait prototypes once
    let mut trait_protos_map: Vec<(&str, HdcVec)> = Vec::new();
    let mut trait_protos_hrr: Vec<(&str, HrrVec)> = Vec::new();
    for other_tr in &["error-handling", "loop", "match", "unsafe", "conditional", "closure", "early-return"] {
        let other_group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(other_tr))
            .map(|(i, _)| i).collect();
        if other_group.len() < 5 { continue; }
        let n = other_group.len().min(50);
        let mv: Vec<HdcVec> = other_group[..n].iter().map(|&i| fns[i].map_strip.clone()).collect();
        trait_protos_map.push((other_tr, hdc::prototype(&mv, map_rng)));
        let hv: Vec<HrrVec> = other_group[..n].iter().map(|&i| fns[i].hrr_strip.clone()).collect();
        trait_protos_hrr.push((other_tr, hrr::bundle(&hv)));
    }

    for tr in &test_traits {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        if group.len() < 20 { continue; }

        let proto_n = group.len() / 2;
        let map_vecs: Vec<HdcVec> = group[..proto_n].iter()
            .map(|&i| fns[i].map_strip.clone()).collect();
        let map_proto = hdc::prototype(&map_vecs, map_rng);

        let hrr_vecs: Vec<HrrVec> = group[..proto_n].iter()
            .map(|&i| fns[i].hrr_strip.clone()).collect();
        let hrr_proto = hrr::bundle(&hrr_vecs);

        let test_n = (group.len() - proto_n).min(50);
        let mut map_hits = 0;
        let mut hrr_hits = 0;

        for &i in group[proto_n..proto_n + test_n].iter() {
            let map_residual = hdc::unbind(&fns[i].map_strip, &map_proto);
            let map_best = trait_protos_map.iter()
                .map(|(label, proto)| (*label, map_residual.cosine_similarity(proto)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            if let Some((best_label, _)) = map_best {
                if best_label == *tr { map_hits += 1; }
            }

            let hrr_residual = fns[i].hrr_strip.unbind(&hrr_proto);
            let hrr_best = trait_protos_hrr.iter()
                .map(|(label, proto)| (*label, hrr_residual.cosine_similarity(proto)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            if let Some((best_label, _)) = hrr_best {
                if best_label == *tr { hrr_hits += 1; }
            }
        }

        println!("  {tr}: MAP unbind→cleanup {map_hits}/{test_n} ({:.0}%), HRR {hrr_hits}/{test_n} ({:.0}%)",
            pct(map_hits, test_n), pct(hrr_hits, test_n));
    }

    let (map_rate, hrr_rate) = decomposition_totals(fns, map_rng);

    println!("\n  Overall: MAP {:.0}%, HRR {:.0}%\n", map_rate, hrr_rate);

    let map_verdict = if map_rate >= 40.0 { Verdict::Works } else if map_rate >= 20.0 { Verdict::NeedsWork } else { Verdict::NotViable };
    let hrr_verdict = if hrr_rate >= 40.0 { Verdict::Works } else if hrr_rate >= 20.0 { Verdict::NeedsWork } else { Verdict::NotViable };

    TestResult {
        name: "Decomposition",
        map_summary: format!("{:.0}% recover", map_rate),
        hrr_summary: format!("{:.0}% recover", hrr_rate),
        verdict_map: map_verdict,
        verdict_hrr: hrr_verdict,
        recommendation: if hrr_rate > map_rate * 1.5 { "HRR" } else if map_rate > hrr_rate { "MAP" } else { "Either" },
    }
}

fn decomposition_totals(fns: &[EncodedFn], map_rng: &mut MapRng) -> (f64, f64) {
    let test_traits = ["error-handling", "loop", "match", "unsafe"];
    let mut map_total = 0usize;
    let mut map_hits = 0usize;
    let mut hrr_total = 0usize;
    let mut hrr_hits = 0usize;

    // Build all trait prototypes
    let mut trait_protos_map: Vec<(&str, HdcVec)> = Vec::new();
    let mut trait_protos_hrr: Vec<(&str, HrrVec)> = Vec::new();
    for tr in &["error-handling", "loop", "match", "unsafe", "conditional", "closure", "early-return"] {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        if group.len() < 5 { continue; }
        let n = group.len().min(50);
        let mv: Vec<HdcVec> = group[..n].iter().map(|&i| fns[i].map_strip.clone()).collect();
        trait_protos_map.push((tr, hdc::prototype(&mv, map_rng)));
        let hv: Vec<HrrVec> = group[..n].iter().map(|&i| fns[i].hrr_strip.clone()).collect();
        trait_protos_hrr.push((tr, hrr::bundle(&hv)));
    }

    for tr in &test_traits {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        if group.len() < 20 { continue; }

        let proto_n = group.len() / 2;
        let map_vecs: Vec<HdcVec> = group[..proto_n].iter()
            .map(|&i| fns[i].map_strip.clone()).collect();
        let map_proto = hdc::prototype(&map_vecs, map_rng);
        let hrr_vecs: Vec<HrrVec> = group[..proto_n].iter()
            .map(|&i| fns[i].hrr_strip.clone()).collect();
        let hrr_proto = hrr::bundle(&hrr_vecs);

        let test_n = (group.len() - proto_n).min(50);
        for &i in group[proto_n..proto_n + test_n].iter() {
            let map_residual = hdc::unbind(&fns[i].map_strip, &map_proto);
            let map_best = trait_protos_map.iter()
                .map(|(label, proto)| (*label, map_residual.cosine_similarity(proto)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            if let Some((best_label, _)) = map_best {
                if best_label == *tr { map_hits += 1; }
            }
            map_total += 1;

            let hrr_residual = fns[i].hrr_strip.unbind(&hrr_proto);
            let hrr_best = trait_protos_hrr.iter()
                .map(|(label, proto)| (*label, hrr_residual.cosine_similarity(proto)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            if let Some((best_label, _)) = hrr_best {
                if best_label == *tr { hrr_hits += 1; }
            }
            hrr_total += 1;
        }
    }

    (pct(map_hits, map_total), pct(hrr_hits, hrr_total))
}

// ===== Test 3: Transform search =====

fn test_transform_search(fns: &[EncodedFn]) -> TestResult {
    println!("=== Test 3: Transform search — \"find the X version of this function\" ===");
    println!("Learn a trait transform from one pair, apply to individuals\n");

    let traits_to_test = ["error-handling", "loop", "match"];
    let mut map_total = 0;
    let mut map_closer = 0;
    let mut hrr_total = 0;
    let mut hrr_closer = 0;

    for tr in &traits_to_test {
        let has: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        let lacks: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| !f.traits.contains(tr))
            .map(|(i, _)| i).collect();

        if has.len() < 20 || lacks.len() < 20 { continue; }

        // Find same-file pairs
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for &h in &has {
            for &l in &lacks {
                if fns[h].file == fns[l].file && pairs.len() < 50 {
                    pairs.push((h, l));
                    break;
                }
            }
        }
        if pairs.len() < 4 { continue; }

        // Learn HRR transform from first pair
        let (ex_has, ex_lacks) = pairs[0];
        let hrr_transform = fns[ex_has].hrr_strip.sub(&fns[ex_lacks].hrr_strip);

        // MAP: can't do subtraction, but can try XOR-based "diff"
        // bind(a, b) gives a "difference" vector in MAP space
        let map_transform = fns[ex_has].map_strip.bind(&fns[ex_lacks].map_strip);

        let mut m_closer = 0;
        let mut h_closer = 0;
        let mut total = 0;

        for &(_, l) in pairs[1..].iter().take(20) {
            // HRR: add transform
            let hrr_transformed = fns[l].hrr_strip.add(&hrr_transform);
            let hrr_sim_has: f64 = has.iter().take(50)
                .map(|&h| hrr_transformed.cosine_similarity(&fns[h].hrr_strip))
                .sum::<f64>() / has.len().min(50) as f64;
            let hrr_sim_lacks: f64 = lacks.iter().take(50)
                .map(|&l2| hrr_transformed.cosine_similarity(&fns[l2].hrr_strip))
                .sum::<f64>() / lacks.len().min(50) as f64;
            if hrr_sim_has > hrr_sim_lacks { h_closer += 1; }

            // MAP: XOR with transform, check similarity
            let map_transformed = fns[l].map_strip.bind(&map_transform);
            let map_sim_has: f64 = has.iter().take(50)
                .map(|&h| map_transformed.cosine_similarity(&fns[h].map_strip))
                .sum::<f64>() / has.len().min(50) as f64;
            let map_sim_lacks: f64 = lacks.iter().take(50)
                .map(|&l2| map_transformed.cosine_similarity(&fns[l2].map_strip))
                .sum::<f64>() / lacks.len().min(50) as f64;
            if map_sim_has > map_sim_lacks { m_closer += 1; }

            total += 1;
        }

        println!("  {tr}: MAP {m_closer}/{total} ({:.0}%), HRR {h_closer}/{total} ({:.0}%)",
            pct(m_closer, total), pct(h_closer, total));

        map_closer += m_closer;
        hrr_closer += h_closer;
        map_total += total;
        hrr_total += total;
    }

    let map_rate = pct(map_closer, map_total);
    let hrr_rate = pct(hrr_closer, hrr_total);

    println!("\n  Overall: MAP {map_rate:.0}%, HRR {hrr_rate:.0}%\n");

    let map_verdict = if map_rate >= 60.0 { Verdict::Works } else if map_rate >= 40.0 { Verdict::NeedsWork } else { Verdict::NotViable };
    let hrr_verdict = if hrr_rate >= 60.0 { Verdict::Works } else if hrr_rate >= 40.0 { Verdict::NeedsWork } else { Verdict::NotViable };

    TestResult {
        name: "Transform search",
        map_summary: format!("{:.0}% correct", map_rate),
        hrr_summary: format!("{:.0}% correct", hrr_rate),
        verdict_map: map_verdict,
        verdict_hrr: hrr_verdict,
        recommendation: if hrr_rate > map_rate + 10.0 { "HRR" } else if map_rate > hrr_rate + 10.0 { "MAP" } else { "Either" },
    }
}

// ===== Test 4: Cross-file structural diff =====

fn test_cross_file_diff(fns: &[EncodedFn]) -> TestResult {
    println!("=== Test 4: Cross-file structural diff ===");
    println!("\"How does module A differ from module B?\"\n");

    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, f) in fns.iter().enumerate() {
        by_file.entry(&f.file).or_default().push(i);
    }

    let modules: Vec<(&str, Vec<usize>)> = by_file.into_iter()
        .filter(|(_, indices)| indices.len() >= 3)
        .collect();

    if modules.len() < 2 {
        println!("  Too few modules with >= 3 functions ({}).\n", modules.len());
        return TestResult {
            name: "Cross-file diff",
            map_summary: "N/A".into(),
            hrr_summary: "N/A".into(),
            verdict_map: Verdict::NotViable,
            verdict_hrr: Verdict::NotViable,
            recommendation: "N/A",
        };
    }

    // Build module prototypes
    let hrr_modules: Vec<(&str, HrrVec)> = modules.iter()
        .map(|(file, indices)| {
            let vecs: Vec<HrrVec> = indices.iter().map(|&i| fns[i].hrr_strip.clone()).collect();
            (*file, hrr::bundle(&vecs))
        })
        .collect();

    // Test: for each pair of modules, unbind one from the other.
    // The residual should be more similar to traits unique to the first module.
    let mut interpretable = 0;
    let mut total_pairs = 0;

    let test_count = modules.len().min(20);
    for i in 0..test_count {
        for j in (i + 1)..test_count {
            // What traits does module i have that j doesn't (and vice versa)?
            let traits_i: HashMap<&str, usize> = count_traits(&modules[i].1, fns);
            let traits_j: HashMap<&str, usize> = count_traits(&modules[j].1, fns);

            let unique_i: Vec<&str> = traits_i.keys()
                .filter(|&&t| {
                    let rate_i = *traits_i.get(t).unwrap_or(&0) as f64 / modules[i].1.len() as f64;
                    let rate_j = *traits_j.get(t).unwrap_or(&0) as f64 / modules[j].1.len() as f64;
                    rate_i > rate_j + 0.2
                })
                .copied().collect();

            if unique_i.is_empty() { continue; }

            // HRR: unbind module j from module i → residual should relate to unique_i traits
            let diff = hrr_modules[i].1.unbind(&hrr_modules[j].1);

            // Check if the diff vector is more similar to functions with unique_i traits
            let with_traits: Vec<usize> = fns.iter().enumerate()
                .filter(|(_, f)| unique_i.iter().any(|t| f.traits.contains(t)))
                .map(|(idx, _)| idx).collect();
            let without_traits: Vec<usize> = fns.iter().enumerate()
                .filter(|(_, f)| !unique_i.iter().any(|t| f.traits.contains(t)))
                .map(|(idx, _)| idx).collect();

            if with_traits.is_empty() || without_traits.is_empty() { continue; }

            let sim_with: f64 = with_traits.iter().take(100)
                .map(|&idx| diff.cosine_similarity(&fns[idx].hrr_strip).abs())
                .sum::<f64>() / with_traits.len().min(100) as f64;
            let sim_without: f64 = without_traits.iter().take(100)
                .map(|&idx| diff.cosine_similarity(&fns[idx].hrr_strip).abs())
                .sum::<f64>() / without_traits.len().min(100) as f64;

            if sim_with > sim_without {
                interpretable += 1;
            }
            total_pairs += 1;
        }
    }

    let hrr_rate = pct(interpretable, total_pairs);
    println!("  Module pairs tested: {total_pairs}");
    println!("  HRR diff interpretable: {interpretable}/{total_pairs} ({hrr_rate:.0}%)");
    println!("  (MAP cannot do subtraction — N/A)\n");

    let hrr_verdict = if hrr_rate >= 60.0 { Verdict::Works } else if hrr_rate >= 40.0 { Verdict::NeedsWork } else { Verdict::NotViable };

    TestResult {
        name: "Cross-file diff",
        map_summary: "N/A".into(),
        hrr_summary: format!("{:.0}% interpret", hrr_rate),
        verdict_map: Verdict::NotViable,
        verdict_hrr: hrr_verdict,
        recommendation: if hrr_rate >= 40.0 { "HRR only" } else { "Not viable yet" },
    }
}

fn count_traits<'a>(indices: &[usize], fns: &'a [EncodedFn]) -> HashMap<&'a str, usize> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for &i in indices {
        for &t in &fns[i].traits {
            *counts.entry(t).or_default() += 1;
        }
    }
    counts
}

// ===== Test 5: Performance budget =====

fn test_performance(
    fn_count: usize,
    map_us: u64,
    hrr_us: u64,
    map_codebook_size: usize,
    hrr_codebook_size: usize,
) -> TestResult {
    println!("=== Test 5: Performance budget ===\n");

    let map_per_fn = map_us as f64 / fn_count as f64;
    let hrr_per_fn = hrr_us as f64 / fn_count as f64;
    let ratio = hrr_us as f64 / map_us.max(1) as f64;

    println!("  Functions encoded: {fn_count}");
    println!("  MAP total: {:.1}ms ({:.1}µs/fn)", map_us as f64 / 1000.0, map_per_fn);
    println!("  HRR total: {:.1}ms ({:.1}µs/fn)", hrr_us as f64 / 1000.0, hrr_per_fn);
    if ratio >= 1.0 {
        println!("  HRR is {ratio:.1}x slower than MAP");
    } else {
        println!("  HRR is {:.1}x FASTER than MAP", 1.0 / ratio);
    }
    println!("  Codebook: MAP {map_codebook_size} entries, HRR {hrr_codebook_size} entries");

    // MAP: 10000 bits = 1250 bytes per vector
    let map_bytes_per_vec = 10000 / 8;
    // HRR: 1024 f64s = 8192 bytes per vector
    let hrr_bytes_per_vec = 1024 * 8;

    let map_storage_mb = (fn_count * 3 * map_bytes_per_vec) as f64 / 1_048_576.0;
    let hrr_storage_mb = (fn_count * 3 * hrr_bytes_per_vec) as f64 / 1_048_576.0;

    println!("  Storage (3 vecs/fn): MAP {map_storage_mb:.2} MB, HRR {hrr_storage_mb:.2} MB");

    // Extrapolation
    println!("\n  Extrapolation:");
    for scale in [1_000, 10_000, 100_000] {
        let map_ms = map_per_fn * scale as f64 / 1000.0;
        let hrr_ms = hrr_per_fn * scale as f64 / 1000.0;
        let map_mb = (scale * 3 * map_bytes_per_vec) as f64 / 1_048_576.0;
        let hrr_mb = (scale * 3 * hrr_bytes_per_vec) as f64 / 1_048_576.0;
        println!("    {scale:>7} fns: MAP {map_ms:>8.1}ms / {map_mb:>6.1}MB, HRR {hrr_ms:>8.1}ms / {hrr_mb:>6.1}MB");
    }

    // Incremental: re-encoding one file ≈ re-encoding ~10 functions
    let incr_map_us = map_per_fn * 10.0;
    let incr_hrr_us = hrr_per_fn * 10.0;
    println!("\n  Incremental (1 file ≈ 10 fns): MAP {incr_map_us:.0}µs, HRR {incr_hrr_us:.0}µs");
    println!();

    let map_viable = map_per_fn < 1000.0; // < 1ms per function
    let hrr_viable = hrr_per_fn < 5000.0; // < 5ms per function (more lenient for the capabilities)

    let map_verdict = if map_viable { Verdict::Works } else { Verdict::NeedsWork };
    let hrr_verdict = if hrr_viable { Verdict::Works } else { Verdict::NeedsWork };

    TestResult {
        name: "Performance",
        map_summary: format!("{:.0}µs/fn", map_per_fn),
        hrr_summary: format!("{:.0}µs/fn ({ratio:.1}x)", hrr_per_fn),
        verdict_map: map_verdict,
        verdict_hrr: hrr_verdict,
        recommendation: if ratio > 10.0 { "MAP for hot path" } else if ratio < 0.5 { "HRR faster" } else { "Both viable" },
    }
}

fn bench_encoding(
    root: &str,
    rs_files: &[PathBuf],
    c_files: &[PathBuf],
) -> (u64, u64) {
    let mut rs_parser = tree_sitter::Parser::new();
    rs_parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
    let mut c_parser = tree_sitter::Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into()).unwrap();

    // Pre-parse all files
    let mut parsed: Vec<(String, tree_sitter::Tree, Lang)> = Vec::new();
    for (files, lang, parser) in [
        (rs_files, Lang::Rust, &mut rs_parser),
        (c_files, Lang::C, &mut c_parser),
    ] {
        for path in files {
            if let Ok(source) = std::fs::read_to_string(path) {
                if let Some(tree) = parser.parse(&source, None) {
                    parsed.push((source, tree, lang));
                }
            }
        }
    }

    // Benchmark MAP
    let t0 = Instant::now();
    let mut cb = MapCodebook::new(42);
    let mut rng = MapRng::new(123);
    for (source, tree, lang) in &parsed {
        for (_, node_id) in extract_function_nodes(tree, source.as_bytes(), *lang) {
            if let Some(node) = find_node_by_id(&tree.root_node(), node_id) {
                let _ = hdc::encode(&node, source.as_bytes(), &mut cb, &mut rng, 5, IdentMode::Strip);
            }
        }
    }
    let map_us = t0.elapsed().as_micros() as u64;

    // Benchmark HRR
    let t0 = Instant::now();
    let mut cb = HrrCodebook::new(42);
    let mut rng = HrrRng::new(123);
    for (source, tree, lang) in &parsed {
        for (_, node_id) in extract_function_nodes(tree, source.as_bytes(), *lang) {
            if let Some(node) = find_node_by_id(&tree.root_node(), node_id) {
                let _ = encode_hrr(&node, source.as_bytes(), &mut cb, &mut rng, 5, false);
            }
        }
    }
    let hrr_us = t0.elapsed().as_micros() as u64;

    let _ = root; // used by caller for context
    (map_us, hrr_us)
}

// ===== HRR encoding (ported from hrr-spike.rs) =====

fn encode_hrr(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut HrrCodebook,
    rng: &mut HrrRng,
    max_depth: usize,
    embed_idents: bool,
) -> HrrVec {
    let kind_vec = codebook.get_or_create(node.kind());

    if max_depth == 0 || node.child_count() == 0 {
        if embed_idents
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
        if !child.is_named() { continue; }
        let child_enc = encode_hrr(&child, source, codebook, rng, max_depth - 1, embed_idents);
        let positional = child_enc.permute(pos + 1);
        child_vecs.push(positional);
        pos += 1;
    }

    if child_vecs.is_empty() {
        return kind_vec;
    }

    let bundled_children = hrr::bundle(&child_vecs);
    kind_vec.bind(&bundled_children)
}

fn encode_bigrams_hrr(ops: &[hdc::Op], codebook: &mut HrrCodebook, _rng: &mut HrrRng) -> Option<HrrVec> {
    if ops.len() < 2 { return None; }
    let bigrams: Vec<HrrVec> = ops.windows(2)
        .map(|w| {
            let a = codebook.get_or_create(&format!("op:{}", w[0].label));
            let b = codebook.get_or_create(&format!("op:{}", w[1].label));
            a.bind(&b.permute(1))
        })
        .collect();
    Some(hrr::bundle(&bigrams))
}

// ===== Utilities (shared with other spike binaries) =====

fn classify_node(node: &tree_sitter::Node, source: &[u8], lang: Lang) -> Vec<&'static str> {
    let mut traits = Vec::new();
    classify_walk(node, source, &mut traits, lang);
    traits.sort();
    traits.dedup();
    traits
}

fn classify_walk(node: &tree_sitter::Node, source: &[u8], traits: &mut Vec<&'static str>, lang: Lang) {
    match (lang, node.kind()) {
        (Lang::Rust, "match_expression") => traits.push("match"),
        (Lang::Rust, "for_expression") | (Lang::Rust, "while_expression") | (Lang::Rust, "loop_expression") => {
            traits.push("loop");
        }
        (Lang::Rust, "if_expression") => traits.push("conditional"),
        (Lang::Rust, "try_expression") => traits.push("error-handling"),
        (Lang::Rust, "unsafe_block") => traits.push("unsafe"),
        (Lang::Rust, "closure_expression") => traits.push("closure"),
        (Lang::Rust, "macro_invocation") => traits.push("macro-call"),
        (Lang::Rust, "call_expression") => {
            if let Some(func) = node.child_by_field_name("function") {
                if let Ok(text) = func.utf8_text(source) {
                    if text.ends_with("unwrap") || text.ends_with("expect") {
                        traits.push("unwrap");
                    }
                }
            }
        }
        (Lang::Rust, "return_expression") => traits.push("early-return"),
        (Lang::C, "switch_statement") => traits.push("match"),
        (Lang::C, "for_statement") | (Lang::C, "while_statement") | (Lang::C, "do_statement") => {
            traits.push("loop");
        }
        (Lang::C, "if_statement") => traits.push("conditional"),
        (Lang::C, "goto_statement") => traits.push("goto"),
        (Lang::C, "return_statement") => traits.push("early-return"),
        (Lang::C, "call_expression") => {
            if let Some(func) = node.child_by_field_name("function") {
                if let Ok(text) = func.utf8_text(source) {
                    if text == "malloc" || text == "calloc" || text == "realloc" || text == "kmalloc" {
                        traits.push("alloc");
                    }
                    if text == "free" || text == "kfree" {
                        traits.push("free");
                    }
                }
            }
        }
        (Lang::C, "pointer-deref") => traits.push("pointer-deref"),
        _ => {}
    }
    for i in 0..node.child_count() {
        classify_walk(&node.child(i).unwrap(), source, traits, lang);
    }
}

fn k_nearest_map(idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HdcVec, k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f64)> = fns.iter().enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(i, f)| (i, get_vec(&fns[idx]).cosine_similarity(get_vec(f))))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored.iter().take(k).map(|(i, _)| *i).collect()
}

fn k_nearest_hrr(idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HrrVec, k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f64)> = fns.iter().enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(i, f)| (i, get_vec(&fns[idx]).cosine_similarity(get_vec(f))))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored.iter().take(k).map(|(i, _)| *i).collect()
}

fn pct(num: usize, denom: usize) -> f64 {
    if denom == 0 { 0.0 } else { 100.0 * num as f64 / denom as f64 }
}

fn extract_function_nodes(tree: &tree_sitter::Tree, source: &[u8], lang: Lang) -> Vec<(String, usize)> {
    let mut results = Vec::new();
    collect_functions(&tree.root_node(), source, &mut results, lang);
    results
}

fn collect_functions(node: &tree_sitter::Node, source: &[u8], out: &mut Vec<(String, usize)>, lang: Lang) {
    let is_fn = match lang {
        Lang::Rust => node.kind() == "function_item" || node.kind() == "function_signature_item",
        Lang::C => node.kind() == "function_definition",
    };
    if is_fn {
        let name = node.child_by_field_name("name")
            .or_else(|| node.child_by_field_name("declarator").and_then(|d| d.child_by_field_name("declarator")))
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("<anon>")
            .to_string();
        out.push((name, node.id()));
    }
    for i in 0..node.child_count() {
        collect_functions(&node.child(i).unwrap(), source, out, lang);
    }
}

fn find_node_by_id<'a>(node: &tree_sitter::Node<'a>, id: usize) -> Option<tree_sitter::Node<'a>> {
    if node.id() == id { return Some(*node); }
    for i in 0..node.child_count() {
        if let Some(found) = find_node_by_id(&node.child(i).unwrap(), id) {
            return Some(found);
        }
    }
    None
}

fn collect_files(dir: &str, ext: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(dir.as_ref(), ext, &mut files);
    files.sort();
    files
}

fn collect_files_recursive(dir: &std::path::Path, ext: &str, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(_) => return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() { collect_files_recursive(&path, ext, out); }
        else if path.extension().is_some_and(|e| e == ext) { out.push(path); }
    }
}

fn short_path(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, f)| f).unwrap_or(path)
}
