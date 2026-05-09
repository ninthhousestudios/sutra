use std::path::PathBuf;
use std::time::Instant;

use sutra::hdc::{self, Codebook, HdcVec, IdentMode, Op, Rng};

struct EncodedFn {
    file: String,
    name: String,
    line_count: usize,
    vec_strip: HdcVec,
    vec_embed: HdcVec,
    ops: Vec<Op>,
    bigram_vec: Option<HdcVec>,
    seq_vec: Option<HdcVec>,
}

#[derive(Clone, Copy)]
enum Lang { Rust, C }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or("src");
    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");

    println!("=== HDC Algebra & Sequence Experiments ===\n");
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
                let vec_embed = hdc::encode(&node, source.as_bytes(), &mut cb_embed, &mut rng, 5, IdentMode::Embed);
                let vec_strip = hdc::encode(&node, source.as_bytes(), &mut cb_strip, &mut rng, 5, IdentMode::Strip);
                let ops = hdc::extract_ops(&node, source.as_bytes());
                let bigram_vec = hdc::encode_bigrams(&ops, &mut cb_seq, &mut rng);
                let seq_vec = hdc::encode_sequence(&ops, &mut cb_seq, &mut rng);
                functions.push(EncodedFn { file: path.display().to_string(), name, line_count, vec_strip, vec_embed, ops, bigram_vec, seq_vec });
            }
        }
    }

    let elapsed = t0.elapsed();
    println!("Encoded {} functions in {:.1}ms\n", functions.len(), elapsed.as_secs_f64() * 1000.0);

    experiment_prototype_retrieval(&functions);
    experiment_unbinding(&functions);
    experiment_sequence_distribution(&functions);
    experiment_bigram_similarity(&functions);
    experiment_security_patterns(&functions);
}

// === Experiment A: Prototype leave-one-out ===

fn experiment_prototype_retrieval(fns: &[EncodedFn]) {
    println!("=== A: Prototype leave-one-out retrieval ===\n");
    println!("Find natural groups, hold one out, bundle rest into prototype,");
    println!("check if held-out member is nearest neighbor of prototype.\n");

    // Find groups by name (functions with identical names in different files)
    let mut by_name: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, f) in fns.iter().enumerate() {
        by_name.entry(&f.name).or_default().push(i);
    }
    let mut groups: Vec<(&str, Vec<usize>)> = by_name.into_iter()
        .filter(|(_, indices)| indices.len() >= 3)
        .collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let mut rng = Rng::new(456);
    let mut total_tests = 0;
    let mut rank1_hits_strip = 0;
    let mut rank5_hits_strip = 0;
    let mut rank1_hits_embed = 0;
    let mut rank5_hits_embed = 0;

    println!("  {:>20} {:>5} {:>8} {:>8} {:>8} {:>8}",
        "group", "size", "R@1(s)", "R@5(s)", "R@1(e)", "R@5(e)");

    for (name, indices) in groups.iter().take(20) {
        let mut g_r1_s = 0; let mut g_r5_s = 0;
        let mut g_r1_e = 0; let mut g_r5_e = 0;

        for (hold_pos, &held_out) in indices.iter().enumerate() {
            let rest_s: Vec<HdcVec> = indices.iter()
                .enumerate()
                .filter(|(p, _)| *p != hold_pos)
                .map(|(_, &i)| fns[i].vec_strip.clone())
                .collect();
            let rest_e: Vec<HdcVec> = indices.iter()
                .enumerate()
                .filter(|(p, _)| *p != hold_pos)
                .map(|(_, &i)| fns[i].vec_embed.clone())
                .collect();

            let proto_s = hdc::prototype(&rest_s, &mut rng);
            let proto_e = hdc::prototype(&rest_e, &mut rng);

            let rank_s = rank_of(&proto_s, held_out, fns, |f| &f.vec_strip);
            let rank_e = rank_of(&proto_e, held_out, fns, |f| &f.vec_embed);

            if rank_s == 0 { g_r1_s += 1; rank1_hits_strip += 1; }
            if rank_s < 5 { g_r5_s += 1; rank5_hits_strip += 1; }
            if rank_e == 0 { g_r1_e += 1; rank1_hits_embed += 1; }
            if rank_e < 5 { g_r5_e += 1; rank5_hits_embed += 1; }
            total_tests += 1;
        }

        let n = indices.len() as f64;
        println!("  {:>20} {:>5} {:>7.0}% {:>7.0}% {:>7.0}% {:>7.0}%",
            name, indices.len(),
            100.0 * g_r1_s as f64 / n, 100.0 * g_r5_s as f64 / n,
            100.0 * g_r1_e as f64 / n, 100.0 * g_r5_e as f64 / n);
    }

    println!("\n  Overall ({total_tests} tests):");
    println!("    Recall@1  — strip: {:.1}%, embed: {:.1}%",
        100.0 * rank1_hits_strip as f64 / total_tests as f64,
        100.0 * rank1_hits_embed as f64 / total_tests as f64);
    println!("    Recall@5  — strip: {:.1}%, embed: {:.1}%",
        100.0 * rank5_hits_strip as f64 / total_tests as f64,
        100.0 * rank5_hits_embed as f64 / total_tests as f64);
    println!();
}

// === Experiment B: Unbinding ===

fn experiment_unbinding(fns: &[EncodedFn]) {
    println!("=== B: Unbinding — compositional decomposition ===\n");
    println!("Build prototype P for a trait group. For function F with that trait,");
    println!("compute residual R = F ⊕ P. Does R capture 'everything else about F'?\n");

    let mut rng = Rng::new(789);

    // Group functions by having specific ops
    let has_lock: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| f.ops.iter().any(|op| op.label == "lock"))
        .map(|(i, _)| i).collect();
    let has_alloc: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| f.ops.iter().any(|op| op.label == "alloc"))
        .map(|(i, _)| i).collect();
    let has_loop: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| f.ops.iter().any(|op| op.label == "loop"))
        .map(|(i, _)| i).collect();

    let groups: Vec<(&str, &[usize])> = vec![
        ("lock", &has_lock),
        ("alloc", &has_alloc),
        ("loop", &has_loop),
    ];

    for (label, group) in &groups {
        if group.len() < 10 { continue; }

        let proto_vecs: Vec<HdcVec> = group.iter().take(50).map(|&i| fns[i].vec_strip.clone()).collect();
        let proto = hdc::prototype(&proto_vecs, &mut rng);

        // For each function in the group, compute residual
        let mut residual_more_general = 0;
        let mut residual_less_specific = 0;
        let mut total = 0;
        let non_group: Vec<usize> = (0..fns.len()).filter(|i| !group.contains(i)).collect();

        for &i in group.iter().skip(50).take(50) {
            let residual = hdc::unbind(&fns[i].vec_strip, &proto);

            // Is the residual more similar to non-group functions than the original was?
            // (meaning we successfully subtracted the group-specific structure)
            let orig_sim_to_nongroup = avg_sim_to_sample(&fns[i].vec_strip, &non_group, fns, |f| &f.vec_strip, 200);
            let resid_sim_to_nongroup = avg_sim_to_sample(&residual, &non_group, fns, |f| &f.vec_strip, 200);

            if resid_sim_to_nongroup > orig_sim_to_nongroup {
                residual_more_general += 1;
            }

            // Is the residual less similar to the prototype than the original?
            let orig_sim_to_proto = fns[i].vec_strip.cosine_similarity(&proto);
            let resid_sim_to_proto = residual.cosine_similarity(&proto);
            if resid_sim_to_proto < orig_sim_to_proto {
                residual_less_specific += 1;
            }
            total += 1;
        }

        if total == 0 { continue; }
        println!("  {label} (n={}, proto from first 50, test on next {total}):", group.len());
        println!("    Residual more similar to non-{label} functions: {residual_more_general}/{total} ({:.0}%)",
            100.0 * residual_more_general as f64 / total as f64);
        println!("    Residual less similar to {label}-prototype:     {residual_less_specific}/{total} ({:.0}%)",
            100.0 * residual_less_specific as f64 / total as f64);
    }
    println!();
}

// === Experiment C: Operation sequence distribution ===

fn experiment_sequence_distribution(fns: &[EncodedFn]) {
    println!("=== C: Operation sequence distribution ===\n");

    let mut op_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut bigram_counts: std::collections::HashMap<(String, String), usize> = std::collections::HashMap::new();
    let mut total_ops = 0;
    let mut fns_with_ops = 0;

    for f in fns {
        if !f.ops.is_empty() {
            fns_with_ops += 1;
        }
        for op in &f.ops {
            *op_counts.entry(&op.label).or_default() += 1;
            total_ops += 1;
        }
        for w in f.ops.windows(2) {
            *bigram_counts.entry((w[0].label.clone(), w[1].label.clone())).or_default() += 1;
        }
    }

    println!("  Functions with ops: {fns_with_ops}/{} ({:.0}%)",
        fns.len(), 100.0 * fns_with_ops as f64 / fns.len() as f64);
    println!("  Total ops extracted: {total_ops}");
    println!("  Avg ops per function: {:.1}\n", total_ops as f64 / fns_with_ops.max(1) as f64);

    let mut sorted_ops: Vec<_> = op_counts.iter().collect();
    sorted_ops.sort_by(|a, b| b.1.cmp(a.1));
    println!("  Top 15 operations:");
    for (op, count) in sorted_ops.iter().take(15) {
        println!("    {count:>6}  {op}");
    }

    let mut sorted_bigrams: Vec<_> = bigram_counts.iter().collect();
    sorted_bigrams.sort_by(|a, b| b.1.cmp(a.1));
    println!("\n  Top 20 bigrams:");
    for ((a, b), count) in sorted_bigrams.iter().take(20) {
        println!("    {count:>6}  {a} → {b}");
    }

    // Security-relevant bigrams
    println!("\n  Security-relevant bigrams:");
    let security_bigrams = [
        ("free", "deref", "use-after-free"),
        ("alloc", "deref", "alloc-then-deref (missing null check?)"),
        ("lock", "return", "lock-then-return (missing unlock?)"),
        ("lock", "goto", "lock-then-goto (missing unlock?)"),
        ("free", "free", "double-free"),
        ("alloc", "return", "alloc-then-return (leak if not stored?)"),
        ("deref", "deref", "chained derefs (null deref chain?)"),
        ("alloc", "alloc", "double alloc (leak?)"),
    ];
    for (a, b, desc) in &security_bigrams {
        let count = bigram_counts.get(&(a.to_string(), b.to_string())).unwrap_or(&0);
        if *count > 0 {
            println!("    {count:>6}  {a} → {b}  ({desc})");
        }
    }
    println!();
}

// === Experiment D: Bigram-encoded similarity ===

fn experiment_bigram_similarity(fns: &[EncodedFn]) {
    println!("=== D: Bigram-encoded similarity ===\n");
    println!("Compare tree-structure similarity vs control-flow similarity.\n");

    let with_bigrams: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| f.bigram_vec.is_some())
        .map(|(i, _)| i).collect();

    if with_bigrams.len() < 20 {
        println!("  Too few functions with bigram vectors ({}).\n", with_bigrams.len());
        return;
    }

    println!("  Functions with bigram vectors: {}\n", with_bigrams.len());

    // For a sample, find nearest neighbor by tree structure vs bigrams
    let sample_size = with_bigrams.len().min(200);
    let mut agree = 0;
    let mut tree_nn_sims = Vec::new();
    let mut bigram_nn_sims = Vec::new();

    for idx in 0..sample_size {
        let i = with_bigrams[idx];
        let best_tree = with_bigrams.iter()
            .filter(|&&j| j != i)
            .map(|&j| (j, fns[i].vec_strip.cosine_similarity(&fns[j].vec_strip)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap();
        let best_bigram = with_bigrams.iter()
            .filter(|&&j| j != i)
            .map(|&j| (j, fns[i].bigram_vec.as_ref().unwrap().cosine_similarity(fns[j].bigram_vec.as_ref().unwrap())))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap();

        if best_tree.0 == best_bigram.0 { agree += 1; }
        tree_nn_sims.push(best_tree.1);
        bigram_nn_sims.push(best_bigram.1);
    }

    let avg_tree: f64 = tree_nn_sims.iter().sum::<f64>() / tree_nn_sims.len() as f64;
    let avg_bigram: f64 = bigram_nn_sims.iter().sum::<f64>() / bigram_nn_sims.len() as f64;

    println!("  NN agreement (tree vs bigram): {agree}/{sample_size} ({:.1}%)",
        100.0 * agree as f64 / sample_size as f64);
    println!("  Avg NN similarity — tree: {avg_tree:.4}, bigram: {avg_bigram:.4}");

    // Show examples where they disagree
    println!("\n  Examples where tree-NN ≠ bigram-NN:");
    let mut shown = 0;
    for idx in 0..sample_size {
        if shown >= 5 { break; }
        let i = with_bigrams[idx];
        let best_tree = with_bigrams.iter()
            .filter(|&&j| j != i)
            .map(|&j| (j, fns[i].vec_strip.cosine_similarity(&fns[j].vec_strip)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap();
        let best_bigram = with_bigrams.iter()
            .filter(|&&j| j != i)
            .map(|&j| (j, fns[i].bigram_vec.as_ref().unwrap().cosine_similarity(fns[j].bigram_vec.as_ref().unwrap())))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap();

        if best_tree.0 != best_bigram.0 {
            let ops_str = |i: usize| -> String {
                let ops: Vec<&str> = fns[i].ops.iter().map(|o| o.label.as_str()).collect();
                if ops.len() > 8 { format!("[{} ops: {}...]", ops.len(), ops[..6].join("→")) }
                else { format!("[{}]", ops.join("→")) }
            };
            println!("    {}::{} {}", short_path(&fns[i].file), fns[i].name, ops_str(i));
            println!("      tree-NN:   {}::{} {}", short_path(&fns[best_tree.0].file), fns[best_tree.0].name, ops_str(best_tree.0));
            println!("      bigram-NN: {}::{} {}", short_path(&fns[best_bigram.0].file), fns[best_bigram.0].name, ops_str(best_bigram.0));
            shown += 1;
        }
    }
    println!();
}

// === Experiment E: Security pattern detection ===

fn experiment_security_patterns(fns: &[EncodedFn]) {
    println!("=== E: Security pattern detection via sequence encoding ===\n");

    // Build prototypes for security-relevant patterns from actual functions
    let has_alloc_no_check: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| {
            let has_alloc = f.ops.iter().any(|o| o.label == "alloc");
            let has_errcheck = f.ops.iter().any(|o| o.label == "errcheck" || o.label == "branch");
            has_alloc && !has_errcheck
        })
        .map(|(i, _)| i).collect();

    let has_alloc_with_check: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| {
            let has_alloc = f.ops.iter().any(|o| o.label == "alloc");
            let has_errcheck = f.ops.iter().any(|o| o.label == "errcheck" || o.label == "branch");
            has_alloc && has_errcheck
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

    let has_free_then_use: Vec<usize> = fns.iter().enumerate()
        .filter(|(_, f)| {
            let mut saw_free = false;
            for op in &f.ops {
                if op.label == "free" { saw_free = true; }
                if saw_free && op.label == "deref" { return true; }
            }
            false
        })
        .map(|(i, _)| i).collect();

    println!("  Pattern counts:");
    println!("    alloc without null check:    {:>5}", has_alloc_no_check.len());
    println!("    alloc with null check:       {:>5}", has_alloc_with_check.len());
    println!("    lock without unlock:         {:>5}", has_lock_no_unlock.len());
    println!("    lock with unlock:            {:>5}", has_lock_with_unlock.len());
    println!("    free-then-deref:             {:>5}", has_free_then_use.len());

    // Show functions matching dangerous patterns
    if !has_free_then_use.is_empty() {
        println!("\n  --- free-then-deref candidates (potential use-after-free) ---");
        for &i in has_free_then_use.iter().take(10) {
            let ops: Vec<&str> = fns[i].ops.iter().map(|o| o.label.as_str()).collect();
            println!("    {}::{} ({} lines)", short_path(&fns[i].file), fns[i].name, fns[i].line_count);
            println!("      ops: {}", ops.join(" → "));
        }
    }

    if !has_lock_no_unlock.is_empty() {
        println!("\n  --- lock-without-unlock candidates (potential deadlock/leak) ---");
        for &i in has_lock_no_unlock.iter().take(10) {
            let ops: Vec<&str> = fns[i].ops.iter().map(|o| o.label.as_str()).collect();
            println!("    {}::{} ({} lines)", short_path(&fns[i].file), fns[i].name, fns[i].line_count);
            println!("      ops: {}", ops.join(" → "));
        }
    }

    if !has_alloc_no_check.is_empty() {
        println!("\n  --- alloc-without-check candidates (potential null deref) ---");
        for &i in has_alloc_no_check.iter().take(10) {
            let ops: Vec<&str> = fns[i].ops.iter().map(|o| o.label.as_str()).collect();
            println!("    {}::{} ({} lines)", short_path(&fns[i].file), fns[i].name, fns[i].line_count);
            println!("      ops: {}", ops.join(" → "));
        }
    }

    // If we have enough of both patterns, build prototypes and test discrimination
    println!("\n  --- Prototype discrimination ---");
    let mut rng = Rng::new(999);

    if has_lock_with_unlock.len() >= 5 && has_lock_no_unlock.len() >= 5 {
        let good_vecs: Vec<_> = has_lock_with_unlock.iter().take(30)
            .filter_map(|&i| fns[i].bigram_vec.as_ref().cloned()).collect();
        let bad_vecs: Vec<_> = has_lock_no_unlock.iter().take(30)
            .filter_map(|&i| fns[i].bigram_vec.as_ref().cloned()).collect();

        if good_vecs.len() >= 3 && bad_vecs.len() >= 3 {
            let proto_good = hdc::prototype(&good_vecs, &mut rng);
            let proto_bad = hdc::prototype(&bad_vecs, &mut rng);

            let test_good: Vec<usize> = has_lock_with_unlock.iter().skip(30).take(50).copied().collect();
            let test_bad: Vec<usize> = has_lock_no_unlock.iter().skip(30).take(50).copied().collect();

            let mut correct = 0;
            let mut total = 0;
            for &i in &test_good {
                if let Some(bv) = &fns[i].bigram_vec {
                    let sim_good = bv.cosine_similarity(&proto_good);
                    let sim_bad = bv.cosine_similarity(&proto_bad);
                    if sim_good > sim_bad { correct += 1; }
                    total += 1;
                }
            }
            for &i in &test_bad {
                if let Some(bv) = &fns[i].bigram_vec {
                    let sim_good = bv.cosine_similarity(&proto_good);
                    let sim_bad = bv.cosine_similarity(&proto_bad);
                    if sim_bad > sim_good { correct += 1; }
                    total += 1;
                }
            }
            if total > 0 {
                println!("    lock-balanced vs lock-unbalanced classification: {correct}/{total} ({:.0}%)",
                    100.0 * correct as f64 / total as f64);
            }
        }
    }

    if has_alloc_with_check.len() >= 5 && has_alloc_no_check.len() >= 5 {
        let good_vecs: Vec<_> = has_alloc_with_check.iter().take(30)
            .filter_map(|&i| fns[i].bigram_vec.as_ref().cloned()).collect();
        let bad_vecs: Vec<_> = has_alloc_no_check.iter().take(30)
            .filter_map(|&i| fns[i].bigram_vec.as_ref().cloned()).collect();

        if good_vecs.len() >= 3 && bad_vecs.len() >= 3 {
            let proto_good = hdc::prototype(&good_vecs, &mut rng);
            let proto_bad = hdc::prototype(&bad_vecs, &mut rng);

            let test_good: Vec<usize> = has_alloc_with_check.iter().skip(30).take(50).copied().collect();
            let test_bad: Vec<usize> = has_alloc_no_check.iter().skip(30).take(50).copied().collect();

            let mut correct = 0;
            let mut total = 0;
            for &i in &test_good {
                if let Some(bv) = &fns[i].bigram_vec {
                    let sim_good = bv.cosine_similarity(&proto_good);
                    let sim_bad = bv.cosine_similarity(&proto_bad);
                    if sim_good > sim_bad { correct += 1; }
                    total += 1;
                }
            }
            for &i in &test_bad {
                if let Some(bv) = &fns[i].bigram_vec {
                    let sim_good = bv.cosine_similarity(&proto_good);
                    let sim_bad = bv.cosine_similarity(&proto_bad);
                    if sim_bad > sim_good { correct += 1; }
                    total += 1;
                }
            }
            if total > 0 {
                println!("    alloc-checked vs alloc-unchecked classification: {correct}/{total} ({:.0}%)",
                    100.0 * correct as f64 / total as f64);
            }
        }
    }
    println!();
}

// === Helpers ===

fn rank_of(query: &HdcVec, target_idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HdcVec) -> usize {
    let target_sim = query.cosine_similarity(get_vec(&fns[target_idx]));
    fns.iter().enumerate()
        .filter(|(i, _)| *i != target_idx)
        .filter(|(_, f)| query.cosine_similarity(get_vec(f)) > target_sim)
        .count()
}

fn avg_sim_to_sample(
    query: &HdcVec, candidates: &[usize], fns: &[EncodedFn],
    get_vec: impl Fn(&EncodedFn) -> &HdcVec, max: usize,
) -> f64 {
    let mut sum = 0.0;
    let count = candidates.len().min(max);
    for &i in candidates.iter().take(count) {
        sum += query.cosine_similarity(get_vec(&fns[i]));
    }
    sum / count.max(1) as f64
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
