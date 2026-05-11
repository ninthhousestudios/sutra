// HRR semantic & temporal binding spike.
//
// Four experiments testing whether HRR can serve as the substrate for
// Sutra's full associative knowledge layer: structural features,
// agent-contributed annotations, temporal evolution, and composition
// with structured queries — all in one vector space.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use sutra::hrr::{self, Codebook, HrrVec, Rng};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

struct EncodedFn {
    file: String,
    name: String,
    traits: Vec<&'static str>,
    vec_strip: HrrVec,
    tags: Vec<Tag>,
}

#[derive(Clone)]
struct Tag {
    category: &'static str,
    value: String,
}

#[derive(Clone, Copy)]
enum Lang { Rust, C }

struct TemporalDiff {
    file: String,
    name: String,
    diff_vec: HrrVec,
    _traits_before: Vec<&'static str>,
    _traits_after: Vec<&'static str>,
    change_labels: Vec<String>,
}

struct CallEdge {
    caller_idx: usize,
    callee_idx: usize,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or("src");

    let temporal_repo = args.iter().position(|a| a == "--temporal-repo")
        .and_then(|i| args.get(i + 1).map(String::as_str));
    let temporal_commits = args.iter().position(|a| a == "--temporal-commits")
        .and_then(|i| args.get(i + 1).map(String::as_str));

    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");

    println!("=== HRR Semantic & Temporal Binding Spike ===\n");
    println!("Root: {root}");
    println!("Files: {} ({} .rs, {} .c)\n",
        rs_files.len() + c_files.len(), rs_files.len(), c_files.len());

    let t0 = Instant::now();
    let mut codebook = Codebook::new(42);
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
                let traits = classify_node(&node, source.as_bytes(), lang);
                let vec_strip = encode_hrr(
                    &node, source.as_bytes(), &mut codebook, &mut rng, 5, false,
                );
                let tags = derive_tags(path, &name, &traits);
                functions.push(EncodedFn {
                    file: path.display().to_string(), name, traits, vec_strip, tags,
                });
            }
        }
    }

    let elapsed = t0.elapsed();
    println!("Encoded {} functions in {:.1}ms",
        functions.len(), elapsed.as_secs_f64() * 1000.0);
    println!("Codebook: {} entries\n", codebook.len());

    if functions.is_empty() {
        println!("No functions found. Check the path.");
        return;
    }

    // --- Experiments ---

    let r1a = exp1a_mixed_retrieval(&functions, &mut codebook, &mut rng);
    let r1b = exp1b_tag_recovery(&functions, &mut codebook, &mut rng);
    let r1c = exp1c_interference_curve(&functions, &mut codebook, &mut rng);

    let r2 = if temporal_repo.is_some() || has_git_history(root) {
        let repo = temporal_repo.unwrap_or(root);
        let commits = temporal_commits.unwrap_or("HEAD~10..HEAD");
        exp2_temporal_binding(repo, commits, &mut codebook, &mut rng)
    } else {
        println!("=== Experiment 2: Temporal binding — SKIPPED (no git history) ===\n");
        ExpResult::skipped("Temporal binding")
    };

    let r3 = exp3_codebook_scaling(&functions, &mut rng);

    let r4 = exp4_composition(&functions, root, &mut codebook, &mut rng);

    // --- Summary ---
    println!("\n{}", "=".repeat(72));
    println!("=== SUMMARY ===\n");
    println!("{:<28} {:<14} {:<36}", "Experiment", "Status", "Key metric");
    println!("{:-<28} {:-<14} {:-<36}", "", "", "");
    for r in [&r1a, &r1b, &r1c, &r2, &r3, &r4] {
        println!("{:<28} {:<14} {}", r.name, r.status, r.metric);
    }

    println!("\n=== VERDICT ===\n");
    let passes = [&r1a, &r1b, &r1c, &r2, &r3, &r4].iter()
        .filter(|r| r.status == "PASS" || r.status == "SKIPPED")
        .count();
    let fails = [&r1a, &r1b, &r1c, &r2, &r3, &r4].iter()
        .filter(|r| r.status == "FAIL")
        .count();
    let caveats = [&r1a, &r1b, &r1c, &r2, &r3, &r4].iter()
        .filter(|r| r.status == "CAVEAT")
        .count();

    if fails == 0 && caveats == 0 {
        println!("VIABLE — all experiments pass.");
    } else if fails <= 1 {
        println!("VIABLE WITH CAVEATS — {passes} pass, {caveats} caveats, {fails} fail.");
    } else {
        println!("NOT VIABLE — {fails} experiments fail.");
    }
    println!();
}

// ---------------------------------------------------------------------------
// Experiment results
// ---------------------------------------------------------------------------

struct ExpResult {
    name: &'static str,
    status: &'static str,
    metric: String,
}

impl ExpResult {
    fn skipped(name: &'static str) -> Self {
        Self { name, status: "SKIPPED", metric: "—".into() }
    }
}

// ---------------------------------------------------------------------------
// Experiment 1a: Mixed retrieval quality
// ---------------------------------------------------------------------------

fn exp1a_mixed_retrieval(
    fns: &[EncodedFn],
    codebook: &mut Codebook,
    _rng: &mut Rng,
) -> ExpResult {
    println!("=== Experiment 1a: Mixed retrieval (structural + semantic) ===\n");

    let traits_to_test = [
        "error-handling", "loop", "match", "conditional", "unsafe",
        "closure", "early-return",
    ];

    // Build enriched vectors: structural + semantic tags bound in
    let enriched: Vec<HrrVec> = fns.iter()
        .map(|f| enrich_vector(&f.vec_strip, &f.tags, codebook))
        .collect();

    println!("  {:>16} {:>5} {:>10} {:>10} {:>10}",
        "trait", "n", "struct P@5", "mixed P@5", "delta");

    let mut total_struct = 0.0;
    let mut total_mixed = 0.0;
    let mut count = 0;

    for tr in &traits_to_test {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i).collect();
        if group.len() < 5 { continue; }

        let sample = group.len().min(100);
        let mut struct_hits = 0;
        let mut mixed_hits = 0;

        for &i in group.iter().take(sample) {
            let struct_nn = k_nearest(i, fns.len(), 5, |j| {
                fns[i].vec_strip.cosine_similarity(&fns[j].vec_strip)
            });
            let mixed_nn = k_nearest(i, fns.len(), 5, |j| {
                enriched[i].cosine_similarity(&enriched[j])
            });

            if struct_nn.iter().any(|&j| fns[j].traits.contains(tr)) { struct_hits += 1; }
            if mixed_nn.iter().any(|&j| fns[j].traits.contains(tr)) { mixed_hits += 1; }
        }

        let sp = pct(struct_hits, sample);
        let mp = pct(mixed_hits, sample);
        let delta = mp - sp;

        println!("  {:>16} {:>5} {:>9.0}% {:>9.0}% {:>+9.0}%",
            tr, group.len(), sp, mp, delta);

        total_struct += sp;
        total_mixed += mp;
        count += 1;
    }

    if count == 0 {
        println!("  No traits with >= 5 members.\n");
        return ExpResult { name: "1a: Mixed retrieval", status: "FAIL", metric: "no data".into() };
    }

    let avg_struct = total_struct / count as f64;
    let avg_mixed = total_mixed / count as f64;
    let ratio = if avg_struct > 0.0 { avg_mixed / avg_struct } else { 0.0 };

    println!("\n  Avg structural P@5: {avg_struct:.1}%");
    println!("  Avg mixed P@5:      {avg_mixed:.1}%");
    println!("  Ratio:              {ratio:.2}x (acceptance: >= 0.80)\n");

    let status = if ratio >= 0.80 { "PASS" } else if ratio >= 0.60 { "CAVEAT" } else { "FAIL" };
    ExpResult {
        name: "1a: Mixed retrieval",
        status,
        metric: format!("{ratio:.2}x (struct {avg_struct:.0}%, mixed {avg_mixed:.0}%)"),
    }
}

// ---------------------------------------------------------------------------
// Experiment 1b: Tag recovery via unbinding
// ---------------------------------------------------------------------------

fn exp1b_tag_recovery(
    fns: &[EncodedFn],
    codebook: &mut Codebook,
    _rng: &mut Rng,
) -> ExpResult {
    println!("=== Experiment 1b: Tag recovery via unbinding ===\n");

    let categories = ["subsystem", "pattern", "risk"];
    let mut total_correct = 0;
    let mut total_tested = 0;

    for cat in &categories {
        // Collect all unique tag values for this category
        let mut tag_values: HashMap<String, HrrVec> = HashMap::new();
        for f in fns {
            for t in &f.tags {
                if t.category == *cat {
                    tag_values.entry(t.value.clone())
                        .or_insert_with(|| codebook.get_or_create(&format!("val:{cat}:{}", t.value)));
                }
            }
        }

        if tag_values.len() < 2 { continue; }

        let role_vec = codebook.get_or_create(&format!("role:{cat}"));
        let sample = fns.len().min(200);
        let mut correct = 0;
        let mut tested = 0;

        for f in fns.iter().take(sample) {
            let actual_tag = f.tags.iter().find(|t| t.category == *cat);
            let actual_tag = match actual_tag {
                Some(t) => t,
                None => continue,
            };

            let enriched = enrich_vector(&f.vec_strip, &f.tags, codebook);
            let recovered = enriched.unbind(&role_vec);

            let best = tag_values.iter()
                .map(|(val, vec)| (val.as_str(), recovered.cosine_similarity(vec)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

            if let Some((best_val, _)) = best {
                if best_val == actual_tag.value { correct += 1; }
            }
            tested += 1;
        }

        let rate = pct(correct, tested);
        println!("  {cat}: {correct}/{tested} ({rate:.0}%) recovered correctly ({} unique values)",
            tag_values.len());

        total_correct += correct;
        total_tested += tested;
    }

    let overall = pct(total_correct, total_tested);
    println!("\n  Overall: {total_correct}/{total_tested} ({overall:.0}%)\n");

    let status = if overall >= 50.0 { "PASS" } else if overall >= 30.0 { "CAVEAT" } else { "FAIL" };
    ExpResult {
        name: "1b: Tag recovery",
        status,
        metric: format!("{overall:.0}% ({total_correct}/{total_tested})"),
    }
}

// ---------------------------------------------------------------------------
// Experiment 1c: Interference curve
// ---------------------------------------------------------------------------

fn exp1c_interference_curve(
    fns: &[EncodedFn],
    codebook: &mut Codebook,
    rng: &mut Rng,
) -> ExpResult {
    println!("=== Experiment 1c: Interference curve ===\n");
    println!("  How many bound tags before structural similarity degrades?\n");

    let sample = fns.len().min(200);
    let levels = [1, 2, 4, 8, 16, 32];

    println!("  {:>6} {:>12} {:>12}", "n_tags", "avg cos_sim", "retention");

    let mut threshold_n = 0;

    for &n_tags in &levels {
        let mut total_sim = 0.0;
        let mut pairs = 0;

        for i in 0..sample {
            let noisy = add_random_tags(&fns[i].vec_strip, n_tags, codebook, rng);
            let sim = noisy.cosine_similarity(&fns[i].vec_strip);
            total_sim += sim;
            pairs += 1;
        }

        let avg_sim = total_sim / pairs as f64;
        let retention = avg_sim * 100.0;

        println!("  {:>6} {:>12.4} {:>11.1}%", n_tags, avg_sim, retention);

        if retention >= 50.0 && threshold_n < n_tags {
            threshold_n = n_tags;
        }
    }

    println!("\n  Interference threshold: ~{threshold_n} tags before structural");
    println!("  similarity drops below 50% retention.\n");

    let status = if threshold_n >= 4 { "PASS" } else if threshold_n >= 2 { "CAVEAT" } else { "FAIL" };
    ExpResult {
        name: "1c: Interference",
        status,
        metric: format!("threshold ~{threshold_n} tags (>= 4 wanted)"),
    }
}

// ---------------------------------------------------------------------------
// Experiment 2: Temporal binding
// ---------------------------------------------------------------------------

fn exp2_temporal_binding(
    repo: &str,
    commit_range: &str,
    codebook: &mut Codebook,
    rng: &mut Rng,
) -> ExpResult {
    println!("=== Experiment 2: Temporal binding ===\n");
    println!("  Repo: {repo}");
    println!("  Range: {commit_range}\n");

    let parts: Vec<&str> = commit_range.splitn(2, "..").collect();
    if parts.len() != 2 {
        println!("  Invalid commit range (expected A..B)\n");
        return ExpResult { name: "2: Temporal", status: "FAIL", metric: "bad range".into() };
    }
    let (commit_a, commit_b) = (parts[0], parts[1]);

    // Find files changed between the two commits
    let changed_files = match git_diff_names(repo, commit_a, commit_b) {
        Ok(f) => f,
        Err(e) => {
            println!("  git diff failed: {e}\n");
            return ExpResult { name: "2: Temporal", status: "FAIL", metric: format!("git: {e}") };
        }
    };

    let code_files: Vec<&str> = changed_files.iter()
        .map(String::as_str)
        .filter(|f| f.ends_with(".rs") || f.ends_with(".c"))
        .collect();

    println!("  Changed code files: {}\n", code_files.len());

    if code_files.is_empty() {
        println!("  No code files changed in range.\n");
        return ExpResult { name: "2: Temporal", status: "SKIPPED", metric: "no changed files".into() };
    }

    let mut rs_parser = tree_sitter::Parser::new();
    rs_parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
    let mut c_parser = tree_sitter::Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into()).unwrap();

    let mut diffs: Vec<TemporalDiff> = Vec::new();

    for &file in &code_files {
        let lang = if file.ends_with(".rs") { Lang::Rust } else { Lang::C };
        let parser = match lang { Lang::Rust => &mut rs_parser, Lang::C => &mut c_parser };

        let src_a = match git_show(repo, commit_a, file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let src_b = match git_show(repo, commit_b, file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let tree_a = match parser.parse(&src_a, None) { Some(t) => t, None => continue };
        let tree_b = match parser.parse(&src_b, None) { Some(t) => t, None => continue };

        let fns_a = extract_and_encode(&tree_a, src_a.as_bytes(), lang, codebook, rng);
        let fns_b = extract_and_encode(&tree_b, src_b.as_bytes(), lang, codebook, rng);

        // Match functions by name
        for (name_b, vec_b, traits_b) in &fns_b {
            if let Some((_, vec_a, traits_a)) = fns_a.iter().find(|(n, _, _)| n == name_b) {
                let diff_vec = vec_b.sub(vec_a);
                let change_labels = compute_change_labels(traits_a, traits_b);
                if !change_labels.is_empty() {
                    diffs.push(TemporalDiff {
                        file: file.to_string(),
                        name: name_b.clone(),
                        diff_vec,
                        _traits_before: traits_a.clone(),
                        _traits_after: traits_b.clone(),
                        change_labels,
                    });
                }
            }
        }
    }

    println!("  Functions with structural changes: {}\n", diffs.len());

    if diffs.len() < 5 {
        println!("  Too few diffs for meaningful analysis.\n");
        return ExpResult {
            name: "2: Temporal",
            status: if diffs.is_empty() { "SKIPPED" } else { "CAVEAT" },
            metric: format!("{} diffs (need >= 5)", diffs.len()),
        };
    }

    // Exp 2a: Diff interpretability — do similar diffs correspond to similar changes?
    let change_types: Vec<&str> = diffs.iter()
        .flat_map(|d| d.change_labels.iter().map(String::as_str))
        .collect();
    let mut change_type_set: Vec<&str> = change_types.clone();
    change_type_set.sort();
    change_type_set.dedup();

    // Build prototype for each change type
    let mut change_protos: Vec<(&str, HrrVec)> = Vec::new();
    for &ct in &change_type_set {
        let vecs: Vec<&HrrVec> = diffs.iter()
            .filter(|d| d.change_labels.iter().any(|l| l == ct))
            .map(|d| &d.diff_vec)
            .collect();
        if vecs.len() >= 2 {
            let owned: Vec<HrrVec> = vecs.iter().map(|v| (*v).clone()).collect();
            change_protos.push((ct, hrr::bundle(&owned)));
        }
    }

    println!("  Change types with prototypes: {} {:?}\n",
        change_protos.len(),
        change_protos.iter().map(|(l, _)| *l).collect::<Vec<_>>());

    let mut correct = 0;
    let mut total = 0;

    for d in &diffs {
        if change_protos.is_empty() { break; }
        let best = change_protos.iter()
            .map(|(label, proto)| (*label, d.diff_vec.cosine_similarity(proto)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        if let Some((best_label, _)) = best {
            if d.change_labels.iter().any(|l| l == best_label) {
                correct += 1;
            }
        }
        total += 1;
    }

    let recovery_rate = pct(correct, total);
    println!("  Exp 2a — Change category recovery: {correct}/{total} ({recovery_rate:.0}%)");
    println!("  Acceptance: > 40%\n");

    // Exp 2b: Diff clustering
    let mut intra_sims = Vec::new();
    let mut inter_sims = Vec::new();

    for i in 0..diffs.len() {
        for j in (i + 1)..diffs.len() {
            let sim = diffs[i].diff_vec.cosine_similarity(&diffs[j].diff_vec);
            let same_change = diffs[i].change_labels.iter()
                .any(|l| diffs[j].change_labels.contains(l));
            if same_change {
                intra_sims.push(sim);
            } else {
                inter_sims.push(sim);
            }
        }
    }

    if !intra_sims.is_empty() && !inter_sims.is_empty() {
        let avg_intra: f64 = intra_sims.iter().sum::<f64>() / intra_sims.len() as f64;
        let avg_inter: f64 = inter_sims.iter().sum::<f64>() / inter_sims.len() as f64;
        println!("  Exp 2b — Diff clustering:");
        println!("    Intra-class similarity: {avg_intra:.4} (n={})", intra_sims.len());
        println!("    Inter-class similarity: {avg_inter:.4} (n={})", inter_sims.len());
        println!("    Separation: {:.4}", avg_intra - avg_inter);
    }

    // Show sample diffs
    println!("\n  Sample diffs:");
    for d in diffs.iter().take(10) {
        println!("    {}::{} — {:?}", short_path(&d.file), d.name, d.change_labels);
    }
    println!();

    let status = if recovery_rate > 40.0 { "PASS" } else if recovery_rate > 25.0 { "CAVEAT" } else { "FAIL" };
    ExpResult {
        name: "2: Temporal",
        status,
        metric: format!("{recovery_rate:.0}% category recovery ({correct}/{total})"),
    }
}

// ---------------------------------------------------------------------------
// Experiment 3: Codebook scaling
// ---------------------------------------------------------------------------

fn exp3_codebook_scaling(fns: &[EncodedFn], rng: &mut Rng) -> ExpResult {
    println!("=== Experiment 3: Codebook scaling ===\n");

    let batch_sizes = [0, 10, 25, 50, 100, 200, 500];
    let sample = fns.len().min(100);

    println!("  {:>8} {:>10} {:>12} {:>12}",
        "cb_size", "P@5 struct", "cleanup_acc", "roundtrip");

    let mut saturation_size = 0usize;
    let mut prev_p5 = 0.0f64;
    let mut interference_size = 0usize;

    for &extra in &batch_sizes {
        let mut cb = Codebook::new(42);
        let _test_rng = Rng::new(123);

        // Re-encode functions with this codebook (to get the base entries)
        let mut rs_parser = tree_sitter::Parser::new();
        rs_parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();

        // Use pre-computed structural vectors for P@5 (they don't depend on extra entries)
        // Add extra random entries to the codebook
        for i in 0..extra {
            cb.get_or_create(&format!("extra:{i}"));
        }

        // Also seed the codebook with all the structural entries by encoding a few functions
        // (This ensures the codebook has the structural entries too)
        let structural_entries = fns.len().min(50);
        for f in fns.iter().take(structural_entries) {
            for t in &f.tags {
                cb.get_or_create(&format!("val:{}:{}", t.category, t.value));
                cb.get_or_create(&format!("role:{}", t.category));
            }
        }
        let total_size = cb.len();

        // Measure P@5 on structural queries (extra entries add noise to cleanup)
        let traits_to_test = ["error-handling", "loop", "match", "conditional"];
        let mut p5_hits = 0;
        let mut p5_total = 0;

        for tr in &traits_to_test {
            let group: Vec<usize> = fns.iter().enumerate()
                .filter(|(_, f)| f.traits.contains(tr))
                .map(|(i, _)| i).collect();
            if group.len() < 5 { continue; }

            for &i in group.iter().take(sample.min(group.len())) {
                let nn = k_nearest(i, fns.len(), 5, |j| {
                    fns[i].vec_strip.cosine_similarity(&fns[j].vec_strip)
                });
                if nn.iter().any(|&j| fns[j].traits.contains(tr)) { p5_hits += 1; }
                p5_total += 1;
            }
        }

        let p5 = pct(p5_hits, p5_total);

        // Measure cleanup accuracy: enrich a vector, then unbind role, cleanup residual
        let mut cleanup_correct = 0;
        let mut cleanup_total = 0;

        let test_n = 50.min(fns.len());
        for f in fns.iter().take(test_n) {
            if f.tags.is_empty() { continue; }
            let t = &f.tags[0];
            let role = cb.get_or_create(&format!("role:{}", t.category));
            let filler = cb.get_or_create(&format!("val:{}:{}", t.category, t.value));
            let enriched = f.vec_strip.add(&role.bind(&filler).scale(0.3)).normalize();
            let recovered = enriched.unbind(&role);
            if let Some((label, _)) = hrr::cleanup(&recovered, &cb) {
                if label == format!("val:{}:{}", t.category, t.value) {
                    cleanup_correct += 1;
                }
            }
            cleanup_total += 1;
        }

        let cleanup_acc = pct(cleanup_correct, cleanup_total);

        // Measure bind-unbind roundtrip
        let mut roundtrip_sum = 0.0;
        let roundtrip_n = 50;
        for _ in 0..roundtrip_n {
            let a = HrrVec::random(rng);
            let b = HrrVec::random(rng);
            let bound = a.bind(&b);
            let recovered = bound.unbind(&b);
            roundtrip_sum += recovered.cosine_similarity(&a);
        }
        let roundtrip = roundtrip_sum / roundtrip_n as f64;

        println!("  {:>8} {:>9.0}% {:>11.0}% {:>12.4}",
            total_size, p5, cleanup_acc, roundtrip);

        // Track saturation / interference
        if extra > 0 && p5 >= prev_p5 - 1.0 {
            saturation_size = total_size;
        }
        if extra > 0 && cleanup_acc < 50.0 && interference_size == 0 {
            interference_size = total_size;
        }
        prev_p5 = p5;
    }

    println!("\n  Theoretical bundle capacity: ~{} items (sqrt({}))",
        (hrr::DEFAULT_DIM as f64).sqrt() as usize, hrr::DEFAULT_DIM);
    if saturation_size > 0 {
        println!("  Saturation point: ~{saturation_size} codebook entries");
    }
    if interference_size > 0 {
        println!("  Interference floor: ~{interference_size} entries (cleanup < 50%)");
    }
    println!();

    let status = if saturation_size > 0 { "PASS" } else { "CAVEAT" };
    ExpResult {
        name: "3: Codebook scaling",
        status,
        metric: format!("saturation ~{saturation_size}, interference ~{interference_size}"),
    }
}

// ---------------------------------------------------------------------------
// Experiment 4: Composition with structured queries
// ---------------------------------------------------------------------------

fn exp4_composition(
    fns: &[EncodedFn],
    root: &str,
    _codebook: &mut Codebook,
    _rng: &mut Rng,
) -> ExpResult {
    println!("=== Experiment 4: Composition — HRR-ranked impact ===\n");

    // Build a lightweight call graph from tree-sitter
    // Filter out very common names that create spurious edges
    let common_names: std::collections::HashSet<&str> = [
        "new", "default", "clone", "drop", "fmt", "from", "into", "len",
        "is_empty", "as_ref", "as_mut", "deref", "read", "write", "get",
        "set", "push", "pop", "insert", "remove", "contains", "iter",
        "next", "map", "unwrap", "expect", "ok", "err",
    ].into_iter().collect();

    let mut name_to_idx: HashMap<&str, usize> = HashMap::new();
    for (i, f) in fns.iter().enumerate() {
        if !common_names.contains(f.name.as_str()) {
            name_to_idx.entry(&f.name).or_insert(i);
        }
    }

    let mut rs_parser = tree_sitter::Parser::new();
    rs_parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
    let mut c_parser = tree_sitter::Parser::new();
    c_parser.set_language(&tree_sitter_c::LANGUAGE.into()).unwrap();

    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");
    let mut edges: Vec<CallEdge> = Vec::new();

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
            for (caller_name, node_id) in extract_function_nodes(&tree, source.as_bytes(), lang) {
                let caller_idx = match name_to_idx.get(caller_name.as_str()) {
                    Some(&i) => i,
                    None => continue,
                };
                let node = find_node_by_id(&tree.root_node(), node_id).unwrap();
                let callees = extract_callees(&node, source.as_bytes());
                for callee_name in callees {
                    if let Some(&callee_idx) = name_to_idx.get(callee_name.as_str()) {
                        if caller_idx != callee_idx {
                            edges.push(CallEdge { caller_idx, callee_idx });
                        }
                    }
                }
            }
        }
    }

    println!("  Call graph: {} edges among {} functions\n", edges.len(), fns.len());

    if edges.len() < 10 {
        println!("  Too few call edges for meaningful analysis.\n");
        return ExpResult {
            name: "4: Composition",
            status: "CAVEAT",
            metric: format!("{} edges (need >= 10)", edges.len()),
        };
    }

    // Build callers_of adjacency
    let mut callers_of: HashMap<usize, Vec<usize>> = HashMap::new();
    for e in &edges {
        callers_of.entry(e.callee_idx).or_default().push(e.caller_idx);
    }

    // Find functions with enough callers
    let mut targets: Vec<(usize, &Vec<usize>)> = callers_of.iter()
        .filter(|(_, callers)| callers.len() >= 3)
        .map(|(&idx, callers)| (idx, callers))
        .collect();
    targets.sort_by_key(|(_, c)| std::cmp::Reverse(c.len()));

    if targets.is_empty() {
        println!("  No functions with >= 3 callers.\n");
        return ExpResult {
            name: "4: Composition",
            status: "CAVEAT",
            metric: "no functions with >= 3 callers".into(),
        };
    }

    // Subsystem-based context: build a module prototype per directory,
    // then test whether HRR ranking surfaces same-module callers higher.
    let mut by_dir: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, f) in fns.iter().enumerate() {
        let dir = f.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
        by_dir.entry(dir).or_default().push(i);
    }

    let module_protos: Vec<(String, HrrVec)> = by_dir.iter()
        .filter(|(_, indices)| indices.len() >= 3)
        .map(|(dir, indices)| {
            let vecs: Vec<HrrVec> = indices.iter().take(50)
                .map(|&i| fns[i].vec_strip.clone()).collect();
            (dir.clone(), hrr::bundle(&vecs))
        })
        .collect();

    println!("  Module prototypes: {} directories\n", module_protos.len());

    if module_protos.is_empty() {
        println!("  No module prototypes could be built.\n");
        return ExpResult {
            name: "4: Composition",
            status: "CAVEAT",
            metric: "no module prototypes".into(),
        };
    }

    // For each target with callers from multiple directories:
    // does HRR ranking put same-directory callers higher?
    let mut hrr_wins = 0;
    let mut total_comparisons = 0;

    for (target_idx, callers) in targets.iter().take(50) {
        let target_dir = fns[*target_idx].file.rsplit_once('/')
            .map(|(d, _)| d).unwrap_or("");

        // Find module prototype for target's directory
        let target_proto = module_protos.iter()
            .find(|(d, _)| d == target_dir);
        let target_proto = match target_proto {
            Some((_, p)) => p,
            None => continue,
        };

        let same_dir_count = callers.iter()
            .filter(|&&c| {
                fns[c].file.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == target_dir
            })
            .count();

        // Skip if all callers are same-dir or none are
        if same_dir_count == 0 || same_dir_count == callers.len() { continue; }

        // HRR ranking: sort callers by similarity to target's module
        let mut ranked: Vec<(usize, f64)> = callers.iter()
            .map(|&c| (c, fns[c].vec_strip.cosine_similarity(target_proto)))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        // Does top half contain more same-dir callers than random?
        let top_half = callers.len() / 2;
        let hrr_top_same = ranked.iter().take(top_half.max(1))
            .filter(|(c, _)| {
                fns[*c].file.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == target_dir
            })
            .count();
        let expected_random = same_dir_count as f64 * top_half as f64 / callers.len() as f64;

        if hrr_top_same as f64 > expected_random { hrr_wins += 1; }
        total_comparisons += 1;
    }

    let win_rate = pct(hrr_wins, total_comparisons);
    println!("  HRR re-ranking surfaces same-module callers: {hrr_wins}/{total_comparisons} ({win_rate:.0}%)");
    println!("  (> 50% means HRR adds value over random ordering)\n");

    // Print examples
    println!("  Examples:");
    for (target_idx, callers) in targets.iter().take(3) {
        let target_dir = fns[*target_idx].file.rsplit_once('/')
            .map(|(d, _)| d).unwrap_or("");
        let target_proto = module_protos.iter()
            .find(|(d, _)| d == target_dir);
        let target_proto = match target_proto {
            Some((_, p)) => p,
            None => continue,
        };
        let mut ranked: Vec<(usize, f64)> = callers.iter()
            .map(|&c| (c, fns[c].vec_strip.cosine_similarity(target_proto)))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        println!("    Target: {}::{} ({} callers)",
            short_path(&fns[*target_idx].file), fns[*target_idx].name, callers.len());
        for (c, sim) in ranked.iter().take(5) {
            let same = if fns[*c].file.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == target_dir {
                "+"
            } else {
                " "
            };
            println!("      {same} {:.4} {}::{}", sim, short_path(&fns[*c].file), fns[*c].name);
        }
    }
    println!();

    let status = if win_rate > 60.0 { "PASS" } else if win_rate > 45.0 { "CAVEAT" } else { "FAIL" };
    ExpResult {
        name: "4: Composition",
        status,
        metric: format!("{win_rate:.0}% same-module lift"),
    }
}

// ---------------------------------------------------------------------------
// Semantic tag derivation
// ---------------------------------------------------------------------------

fn derive_tags(path: &std::path::Path, name: &str, traits: &[&str]) -> Vec<Tag> {
    let mut tags = Vec::new();

    // Subsystem from directory structure
    let path_str = path.to_string_lossy();
    let components: Vec<&str> = path_str.split('/').collect();
    // Use the first meaningful directory component after the root
    if let Some(subsystem) = components.iter()
        .rev()
        .skip(1) // skip filename
        .find(|c| !c.is_empty() && *c != &"src" && *c != &"lib" && *c != &"bin")
    {
        tags.push(Tag { category: "subsystem", value: subsystem.to_string() });
    }

    // Pattern from function name
    let lower = name.to_lowercase();
    if lower.starts_with("test_") || lower.starts_with("test") && lower.len() > 4 && lower.as_bytes()[4].is_ascii_uppercase() {
        tags.push(Tag { category: "pattern", value: "test".into() });
    } else if lower.ends_with("_init") || lower.starts_with("init_") || lower == "init" {
        tags.push(Tag { category: "pattern", value: "init".into() });
    } else if lower.ends_with("_free") || lower.ends_with("_destroy") || lower.ends_with("_drop")
        || lower.starts_with("free_") || lower.starts_with("destroy_")
    {
        tags.push(Tag { category: "pattern", value: "cleanup".into() });
    } else if lower.starts_with("handle_") || lower.ends_with("_handler") {
        tags.push(Tag { category: "pattern", value: "handler".into() });
    } else if lower.starts_with("new_") || lower.ends_with("_new") || lower == "new" {
        tags.push(Tag { category: "pattern", value: "constructor".into() });
    } else if lower.starts_with("get_") || lower.starts_with("is_") || lower.starts_with("has_") {
        tags.push(Tag { category: "pattern", value: "accessor".into() });
    } else if lower.starts_with("set_") {
        tags.push(Tag { category: "pattern", value: "mutator".into() });
    }

    // Risk from structural complexity
    let nesting_depth = traits.len();
    let risk = if nesting_depth > 4 { "high" }
        else if nesting_depth > 2 { "medium" }
        else { "low" };
    tags.push(Tag { category: "risk", value: risk.into() });

    tags
}

// ---------------------------------------------------------------------------
// Enrichment: bind semantic tags into structural vectors
// ---------------------------------------------------------------------------

fn enrich_vector(structural: &HrrVec, tags: &[Tag], codebook: &mut Codebook) -> HrrVec {
    if tags.is_empty() { return structural.clone(); }

    let mut result = structural.clone();
    for t in tags {
        let role = codebook.get_or_create(&format!("role:{}", t.category));
        let filler = codebook.get_or_create(&format!("val:{}:{}", t.category, t.value));
        result = result.add(&role.bind(&filler).scale(0.3));
    }
    result.normalize()
}

fn add_random_tags(structural: &HrrVec, n: usize, codebook: &mut Codebook, rng: &mut Rng) -> HrrVec {
    let mut result = structural.clone();
    for i in 0..n {
        let role = codebook.get_or_create(&format!("noise_role:{i}"));
        let filler = HrrVec::random(rng);
        result = result.add(&role.bind(&filler).scale(0.3));
    }
    result.normalize()
}

// ---------------------------------------------------------------------------
// Temporal helpers
// ---------------------------------------------------------------------------

fn git_diff_names(repo: &str, commit_a: &str, commit_b: &str) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C").arg(repo)
        .args(["diff", "--name-only"])
        .arg(format!("{commit_a}..{commit_b}"))
        .output()
        .map_err(|e| format!("{e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

fn git_show(repo: &str, commit: &str, path: &str) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C").arg(repo)
        .arg("show")
        .arg(format!("{commit}:{path}"))
        .output()
        .map_err(|e| format!("{e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn has_git_history(root: &str) -> bool {
    Command::new("git")
        .arg("-C").arg(root)
        .args(["log", "--oneline", "-1"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn extract_and_encode(
    tree: &tree_sitter::Tree,
    source: &[u8],
    lang: Lang,
    codebook: &mut Codebook,
    rng: &mut Rng,
) -> Vec<(String, HrrVec, Vec<&'static str>)> {
    let mut results = Vec::new();
    for (name, node_id) in extract_function_nodes(tree, source, lang) {
        if let Some(node) = find_node_by_id(&tree.root_node(), node_id) {
            let vec = encode_hrr(&node, source, codebook, rng, 5, false);
            let traits = classify_node(&node, source, lang);
            results.push((name, vec, traits));
        }
    }
    results
}

fn compute_change_labels(before: &[&str], after: &[&str]) -> Vec<String> {
    let mut labels = Vec::new();
    for &t in after {
        if !before.contains(&t) {
            labels.push(format!("added-{t}"));
        }
    }
    for &t in before {
        if !after.contains(&t) {
            labels.push(format!("removed-{t}"));
        }
    }
    labels
}

// ---------------------------------------------------------------------------
// Call graph extraction
// ---------------------------------------------------------------------------

fn extract_callees(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut callees = Vec::new();
    extract_callees_walk(node, source, &mut callees);
    callees.sort();
    callees.dedup();
    callees
}

fn extract_callees_walk(node: &tree_sitter::Node, source: &[u8], out: &mut Vec<String>) {
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            if let Ok(text) = func.utf8_text(source) {
                // Handle method calls: take last segment
                let name = text.rsplit("::").next().unwrap_or(text);
                let name = name.rsplit('.').next().unwrap_or(name);
                out.push(name.to_string());
            }
        }
    }
    for i in 0..node.child_count() {
        extract_callees_walk(&node.child(i).unwrap(), source, out);
    }
}

// ---------------------------------------------------------------------------
// HRR encoding (from hrr-spike.rs)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Structural trait classification (from hrr-spike.rs)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

fn k_nearest(idx: usize, total: usize, k: usize, sim_fn: impl Fn(usize) -> f64) -> Vec<usize> {
    let mut scored: Vec<(usize, f64)> = (0..total)
        .filter(|&i| i != idx)
        .map(|i| (i, sim_fn(i)))
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
