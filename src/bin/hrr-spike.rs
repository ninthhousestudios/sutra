use std::path::PathBuf;
use std::time::Instant;

use sutra::hrr::{self, Codebook, HrrVec, Rng};

struct EncodedFn {
    file: String,
    name: String,
    _line_count: usize,
    traits: Vec<&'static str>,
    vec_strip: HrrVec,
    vec_embed: HrrVec,
    ops: Vec<sutra::hdc::Op>,
    bigram_vec: Option<HrrVec>,
}

#[derive(Clone, Copy)]
enum Lang { Rust, C }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or("src");

    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");

    println!("=== HRR AST Encoding Spike ===\n");
    println!("Root: {root}");
    println!("Files: {} ({} .rs, {} .c)\n", rs_files.len() + c_files.len(), rs_files.len(), c_files.len());

    let t0 = Instant::now();
    let mut cb_embed = Codebook::new(42);
    let mut cb_strip = Codebook::new(42);
    let mut cb_seq = Codebook::new(99);
    let mut rng = Rng::new(123);
    let mut functions: Vec<EncodedFn> = Vec::new();

    let mut rs_parser = tree_sitter::Parser::new();
    rs_parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
    let mut c_parser = tree_sitter::Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into()).unwrap();

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
                let line_count = node.end_position().row - node.start_position().row + 1;
                let traits = classify_node(&node, source.as_bytes(), lang);
                let vec_embed = encode_hrr(&node, source.as_bytes(), &mut cb_embed, &mut rng, 5, true);
                let vec_strip = encode_hrr(&node, source.as_bytes(), &mut cb_strip, &mut rng, 5, false);
                let ops = sutra::hdc::extract_ops(&node, source.as_bytes());
                let bigram_vec = encode_bigrams_hrr(&ops, &mut cb_seq, &mut rng);
                functions.push(EncodedFn {
                    file: path.display().to_string(), name, _line_count: line_count, traits,
                    vec_embed, vec_strip, ops, bigram_vec,
                });
            }
        }
    }

    let elapsed = t0.elapsed();
    println!("Encoded {} functions in {:.1}ms", functions.len(), elapsed.as_secs_f64() * 1000.0);
    println!("Codebook: {} entries (embed), {} entries (strip)\n", cb_embed.len(), cb_strip.len());

    if functions.is_empty() {
        println!("No functions found. Check the path.");
        return;
    }

    experiment_similarity_search(&functions);
    experiment_structural_traits(&functions);
    experiment_unbind_decomposition(&functions, &mut rng);
    experiment_analogy_real_code(&functions, &mut rng);
    experiment_multiscale(&functions, &mut rng);
    experiment_security_patterns(&functions, &mut rng);
}

// --- HRR tree encoding (ported from hdc.rs) ---

fn encode_hrr(
    node: &tree_sitter::Node,
    source: &[u8],
    codebook: &mut Codebook,
    rng: &mut Rng,
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

fn encode_bigrams_hrr(ops: &[sutra::hdc::Op], codebook: &mut Codebook, _rng: &mut Rng) -> Option<HrrVec> {
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

// --- Experiments ---

fn experiment_similarity_search(fns: &[EncodedFn]) {
    println!("=== 1: Similarity search quality ===\n");

    let sample = fns.len().min(300);
    let mut embed_sims = Vec::new();
    let mut strip_sims = Vec::new();
    let mut agree = 0;

    for i in 0..sample {
        let best_e = nearest_neighbor(i, fns, |f| &f.vec_embed);
        let best_s = nearest_neighbor(i, fns, |f| &f.vec_strip);
        embed_sims.push(best_e.1);
        strip_sims.push(best_s.1);
        if best_e.0 == best_s.0 { agree += 1; }
    }

    let avg_e: f64 = embed_sims.iter().sum::<f64>() / embed_sims.len() as f64;
    let avg_s: f64 = strip_sims.iter().sum::<f64>() / strip_sims.len() as f64;

    println!("  Strip/embed NN agreement: {agree}/{sample} ({:.1}%)", pct(agree, sample));
    println!("  Avg NN similarity — embed: {avg_e:.4}, strip: {avg_s:.4}\n");
}

fn experiment_structural_traits(fns: &[EncodedFn]) {
    println!("=== 2: Structural trait retrieval (P@k) ===\n");

    let traits_to_test = [
        "error-handling", "conditional", "loop", "match", "unsafe",
        "closure", "early-return",
    ];

    println!("  {:>16} {:>5} {:>8} {:>8} {:>8} {:>10}",
        "trait", "n", "P@1", "P@5", "P@10", "base_rate");

    for tr in &traits_to_test {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        if group.len() < 5 { continue; }

        let base_rate = group.len() as f64 / fns.len() as f64;
        let mut p1 = 0; let mut p5 = 0; let mut p10 = 0;
        let sample = group.len().min(100);

        for &i in group.iter().take(sample) {
            let neighbors = k_nearest(i, fns, |f| &f.vec_strip, 10);
            if fns[neighbors[0]].traits.contains(tr) { p1 += 1; }
            let in5 = neighbors[..5].iter().filter(|&&j| fns[j].traits.contains(tr)).count();
            let in10 = neighbors.iter().filter(|&&j| fns[j].traits.contains(tr)).count();
            if in5 > 0 { p5 += 1; }
            if in10 > 0 { p10 += 1; }
        }

        println!("  {:>16} {:>5} {:>7.0}% {:>7.0}% {:>7.0}% {:>9.0}%",
            tr, group.len(), pct(p1, sample), pct(p5, sample), pct(p10, sample),
            base_rate * 100.0);
    }
    println!();
}

fn experiment_unbind_decomposition(fns: &[EncodedFn], _rng: &mut Rng) {
    println!("=== 3: Unbinding — compositional decomposition (HRR advantage) ===\n");
    println!("Build prototype for a trait group. Unbind from function.");
    println!("Does the residual become more general (less trait-specific)?\n");

    let trait_groups: Vec<(&str, Vec<usize>)> = ["error-handling", "loop", "match", "unsafe"]
        .iter()
        .map(|&tr| {
            let indices: Vec<usize> = fns.iter().enumerate()
                .filter(|(_, f)| f.traits.contains(&tr))
                .map(|(i, _)| i).collect();
            (tr, indices)
        })
        .filter(|(_, g)| g.len() >= 20)
        .collect();

    for (label, group) in &trait_groups {
        let proto_n = group.len().min(50);
        let proto_vecs: Vec<HrrVec> = group[..proto_n].iter()
            .map(|&i| fns[i].vec_strip.clone()).collect();
        let proto = hrr::bundle(&proto_vecs);

        let test_start = proto_n;
        let test_n = (group.len() - proto_n).min(50);
        if test_n < 5 { continue; }

        let non_group: Vec<usize> = (0..fns.len())
            .filter(|i| !group.contains(i)).collect();

        let mut residual_less_specific = 0;
        let mut residual_more_general = 0;

        for &i in group[test_start..test_start + test_n].iter() {
            let residual = fns[i].vec_strip.unbind(&proto);

            let orig_sim = fns[i].vec_strip.cosine_similarity(&proto);
            let resid_sim = residual.cosine_similarity(&proto);
            if resid_sim.abs() < orig_sim.abs() {
                residual_less_specific += 1;
            }

            let orig_cross = avg_sim_to_sample(&fns[i].vec_strip, &non_group, fns, 200);
            let resid_cross = avg_sim_to_sample(&residual, &non_group, fns, 200);
            if resid_cross > orig_cross {
                residual_more_general += 1;
            }
        }

        println!("  {label} (n={}, proto from {proto_n}, test on {test_n}):", group.len());
        println!("    Residual less similar to prototype: {residual_less_specific}/{test_n} ({:.0}%)",
            pct(residual_less_specific, test_n));
        println!("    Residual more general (cross-sim):  {residual_more_general}/{test_n} ({:.0}%)",
            pct(residual_more_general, test_n));
    }
    println!();
}

fn experiment_analogy_real_code(fns: &[EncodedFn], _rng: &mut Rng) {
    println!("=== 4: Analogical reasoning on real functions ===\n");
    println!("Learn a trait transformation from examples, apply to new code.\n");

    // For each trait pair (has-trait, lacks-trait), learn the "add trait" transform
    // from one example, apply to another, check if the result is closer to
    // functions that actually have the trait.

    let traits_to_test = ["error-handling", "loop", "match", "conditional"];

    for tr in &traits_to_test {
        let has: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        let lacks: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| !f.traits.contains(tr))
            .map(|(i, _)| i).collect();

        if has.len() < 20 || lacks.len() < 20 { continue; }

        // Find paired functions: same file, one has trait, one doesn't
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

        // Learn transform from first pair
        let (exemplar_has, exemplar_lacks) = pairs[0];
        let transform = fns[exemplar_has].vec_strip.sub(&fns[exemplar_lacks].vec_strip);

        // Apply to remaining functions that lack the trait
        let mut closer_to_has = 0;
        let mut total = 0;

        for &(_, l) in pairs[1..].iter().take(20) {
            let transformed = fns[l].vec_strip.add(&transform);

            let avg_sim_has = has.iter().take(50)
                .map(|&h| transformed.cosine_similarity(&fns[h].vec_strip))
                .sum::<f64>() / has.len().min(50) as f64;
            let avg_sim_lacks = lacks.iter().take(50)
                .map(|&l2| transformed.cosine_similarity(&fns[l2].vec_strip))
                .sum::<f64>() / lacks.len().min(50) as f64;

            if avg_sim_has > avg_sim_lacks { closer_to_has += 1; }
            total += 1;
        }

        println!("  {tr}: transform learned from {}/{}",
            short_path(&fns[exemplar_has].file), fns[exemplar_has].name);
        println!("    Applied to {total} functions: {closer_to_has}/{total} ({:.0}%) moved closer to {tr}-group",
            pct(closer_to_has, total));
    }
    println!();
}

fn experiment_multiscale(fns: &[EncodedFn], _rng: &mut Rng) {
    println!("=== 5: Multi-scale composition ===\n");
    println!("Bundle functions by file → \"module\" vectors. Test retrieval.\n");

    // Group functions by file
    let mut by_file: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, f) in fns.iter().enumerate() {
        by_file.entry(&f.file).or_default().push(i);
    }

    let modules: Vec<(&str, Vec<usize>, HrrVec)> = by_file.iter()
        .filter(|(_, indices)| indices.len() >= 3)
        .map(|(&file, indices)| {
            let vecs: Vec<HrrVec> = indices.iter().map(|&i| fns[i].vec_strip.clone()).collect();
            (file, indices.clone(), hrr::bundle(&vecs))
        })
        .collect();

    if modules.is_empty() {
        println!("  No files with >= 3 functions.\n");
        return;
    }

    // Test 1: can we identify which module a function belongs to?
    let mut correct = 0;
    let mut total = 0;
    let sample = fns.len().min(500);

    for i in 0..sample {
        let true_file = &fns[i].file;
        if let Some(best_mod) = modules.iter()
            .max_by(|a, b| {
                a.2.cosine_similarity(&fns[i].vec_strip)
                    .partial_cmp(&b.2.cosine_similarity(&fns[i].vec_strip))
                    .unwrap()
            })
        {
            if best_mod.0 == true_file { correct += 1; }
            total += 1;
        }
    }

    println!("  Module attribution: {correct}/{total} ({:.1}%) functions matched to correct file",
        pct(correct, total));

    // Test 2: module-level similarity (files with similar functions should cluster)
    let mod_count = modules.len().min(50);
    let mut same_dir_sim = Vec::new();
    let mut diff_dir_sim = Vec::new();

    for i in 0..mod_count {
        for j in (i + 1)..mod_count {
            let sim = modules[i].2.cosine_similarity(&modules[j].2);
            let dir_i = modules[i].0.rsplit_once('/').map(|p| p.0).unwrap_or("");
            let dir_j = modules[j].0.rsplit_once('/').map(|p| p.0).unwrap_or("");
            if dir_i == dir_j && !dir_i.is_empty() {
                same_dir_sim.push(sim);
            } else {
                diff_dir_sim.push(sim);
            }
        }
    }

    if !same_dir_sim.is_empty() && !diff_dir_sim.is_empty() {
        let avg_same: f64 = same_dir_sim.iter().sum::<f64>() / same_dir_sim.len() as f64;
        let avg_diff: f64 = diff_dir_sim.iter().sum::<f64>() / diff_dir_sim.len() as f64;
        println!("  Same-dir module similarity:  {avg_same:.4} (n={})", same_dir_sim.len());
        println!("  Cross-dir module similarity: {avg_diff:.4} (n={})", diff_dir_sim.len());
    }

    // Test 3: function retrieval from module via unbinding
    println!("\n  Function retrieval from module bundles:");
    let mut unbind_hits = 0;
    let mut unbind_total = 0;
    for (_, indices, module_vec) in modules.iter().take(20) {
        if indices.len() < 3 || indices.len() > 20 { continue; }
        for &i in indices.iter().take(5) {
            let recovered = module_vec.unbind(&fns[i].vec_strip);
            // The recovered vector should be more similar to other functions
            // in the same module than to random functions
            let same_mod_sim: f64 = indices.iter()
                .filter(|&&j| j != i)
                .take(5)
                .map(|&j| recovered.cosine_similarity(&fns[j].vec_strip))
                .sum::<f64>() / (indices.len() - 1).min(5) as f64;

            let random_indices: Vec<usize> = (0..fns.len())
                .filter(|j| !indices.contains(j))
                .take(10)
                .collect();
            let random_sim: f64 = random_indices.iter()
                .map(|&j| recovered.cosine_similarity(&fns[j].vec_strip))
                .sum::<f64>() / random_indices.len().max(1) as f64;

            if same_mod_sim > random_sim { unbind_hits += 1; }
            unbind_total += 1;
        }
    }
    if unbind_total > 0 {
        println!("    Unbind recovers module-mates: {unbind_hits}/{unbind_total} ({:.0}%)",
            pct(unbind_hits, unbind_total));
    }
    println!();
}

fn experiment_security_patterns(fns: &[EncodedFn], _rng: &mut Rng) {
    println!("=== 6: Security patterns (sequence encoding) ===\n");

    let with_bigrams: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| f.bigram_vec.is_some())
        .map(|(i, _)| i).collect();

    if with_bigrams.len() < 20 {
        println!("  Too few functions with bigrams ({}).\n", with_bigrams.len());
        return;
    }

    // Pattern counts
    let has_free_then_deref: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| {
            let mut saw_free = false;
            for op in &f.ops {
                if op.label == "free" { saw_free = true; }
                if saw_free && op.label == "deref" { return true; }
            }
            false
        })
        .map(|(i, _)| i).collect();

    let has_lock_no_unlock: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| {
            let locks = f.ops.iter().filter(|o| o.label == "lock").count();
            let unlocks = f.ops.iter().filter(|o| o.label == "unlock").count();
            locks > 0 && unlocks < locks
        })
        .map(|(i, _)| i).collect();

    let has_lock_with_unlock: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| {
            let locks = f.ops.iter().filter(|o| o.label == "lock").count();
            let unlocks = f.ops.iter().filter(|o| o.label == "unlock").count();
            locks > 0 && unlocks >= locks
        })
        .map(|(i, _)| i).collect();

    println!("  Bigram vectors: {}", with_bigrams.len());
    println!("  free→deref:        {}", has_free_then_deref.len());
    println!("  lock-no-unlock:    {}", has_lock_no_unlock.len());
    println!("  lock-with-unlock:  {}\n", has_lock_with_unlock.len());

    // Lock-balanced classifier using HRR prototypes
    if has_lock_with_unlock.len() >= 5 && has_lock_no_unlock.len() >= 5 {
        let good_vecs: Vec<_> = has_lock_with_unlock.iter().take(30)
            .filter_map(|&i| fns[i].bigram_vec.clone()).collect();
        let bad_vecs: Vec<_> = has_lock_no_unlock.iter().take(30)
            .filter_map(|&i| fns[i].bigram_vec.clone()).collect();

        if good_vecs.len() >= 3 && bad_vecs.len() >= 3 {
            let proto_good = hrr::bundle(&good_vecs);
            let proto_bad = hrr::bundle(&bad_vecs);

            let test_good: Vec<usize> = has_lock_with_unlock.iter().skip(30).take(50).copied().collect();
            let test_bad: Vec<usize> = has_lock_no_unlock.iter().skip(30).take(50).copied().collect();

            let mut correct = 0;
            let mut total = 0;
            for &i in test_good.iter().chain(test_bad.iter()) {
                if let Some(bv) = &fns[i].bigram_vec {
                    let is_good = has_lock_with_unlock.contains(&i);
                    let sim_good = bv.cosine_similarity(&proto_good);
                    let sim_bad = bv.cosine_similarity(&proto_bad);
                    let predicted_good = sim_good > sim_bad;
                    if predicted_good == is_good { correct += 1; }
                    total += 1;
                }
            }
            if total > 0 {
                println!("  Lock-balanced classifier: {correct}/{total} ({:.0}%)", pct(correct, total));
            }
        }
    }

    // Show free-then-deref candidates
    if !has_free_then_deref.is_empty() {
        println!("\n  free→deref candidates (top 5):");
        for &i in has_free_then_deref.iter().take(5) {
            let ops: Vec<&str> = fns[i].ops.iter().map(|o| o.label.as_str()).collect();
            println!("    {}::{} — {}", short_path(&fns[i].file), fns[i].name, ops.join("→"));
        }
    }
    println!();
}

// --- Structural trait classification (from hdc-spike.rs) ---

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
        (Lang::C, "pointer_expression") => traits.push("pointer-deref"),
        _ => {}
    }
    for i in 0..node.child_count() {
        classify_walk(&node.child(i).unwrap(), source, traits, lang);
    }
}

// --- Helpers ---

fn nearest_neighbor(idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HrrVec) -> (usize, f64) {
    fns.iter().enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(i, f)| (i, get_vec(&fns[idx]).cosine_similarity(get_vec(f))))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap()
}

fn k_nearest(idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HrrVec, k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f64)> = fns.iter().enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(i, f)| (i, get_vec(&fns[idx]).cosine_similarity(get_vec(f))))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored.iter().take(k).map(|(i, _)| *i).collect()
}

fn avg_sim_to_sample(query: &HrrVec, candidates: &[usize], fns: &[EncodedFn], max: usize) -> f64 {
    let count = candidates.len().min(max);
    if count == 0 { return 0.0; }
    let sum: f64 = candidates.iter().take(count)
        .map(|&i| query.cosine_similarity(&fns[i].vec_strip))
        .sum();
    sum / count as f64
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
