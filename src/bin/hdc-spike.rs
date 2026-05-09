use std::path::PathBuf;
use std::time::Instant;

use sutra::hdc::{self, Codebook, HdcVec, IdentMode, Rng};

struct EncodedFn {
    file: String,
    name: String,
    line_count: usize,
    traits: Vec<&'static str>,
    vec_embed: HdcVec,
    vec_strip: HdcVec,
}

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    C,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or("src");

    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");
    let total_files = rs_files.len() + c_files.len();

    println!("=== HDC AST Encoding Spike ===\n");
    println!("Root: {root}");
    println!("Files: {} ({} .rs, {} .c)\n", total_files, rs_files.len(), c_files.len());

    let t0 = Instant::now();
    let mut cb_embed = Codebook::new(42);
    let mut cb_strip = Codebook::new(42);
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
                let vec_embed = hdc::encode(
                    &node, source.as_bytes(), &mut cb_embed, &mut rng, 5, IdentMode::Embed,
                );
                let vec_strip = hdc::encode(
                    &node, source.as_bytes(), &mut cb_strip, &mut rng, 5, IdentMode::Strip,
                );
                functions.push(EncodedFn { file: path.display().to_string(), name, line_count, traits, vec_embed, vec_strip });
            }
        }
    }

    let elapsed = t0.elapsed();
    println!("Encoded {} functions in {:.1}ms", functions.len(), elapsed.as_secs_f64() * 1000.0);
    println!("Codebook: {} entries (embed), {} entries (strip)\n", cb_embed.len(), cb_strip.len());

    experiment_mode_comparison(&functions);
    experiment_discrimination(&functions);
    experiment_structural_traits(&functions);
    experiment_precision_recall(&functions);
    experiment_query_by_example(&functions);
}

// === Structural trait classification ===

fn classify_node(node: &tree_sitter::Node, source: &[u8], lang: Lang) -> Vec<&'static str> {
    let mut traits = Vec::new();
    let mut ctx = ClassifyCtx { source, traits: &mut traits, lang };
    ctx.walk(node);
    traits.sort();
    traits.dedup();
    traits
}

struct ClassifyCtx<'a> {
    source: &'a [u8],
    traits: &'a mut Vec<&'static str>,
    lang: Lang,
}

impl ClassifyCtx<'_> {
    fn walk(&mut self, node: &tree_sitter::Node) {
        match (self.lang, node.kind()) {
            // Rust traits
            (Lang::Rust, "match_expression") => self.traits.push("match"),
            (Lang::Rust, "for_expression") | (Lang::Rust, "while_expression") | (Lang::Rust, "loop_expression") => {
                self.traits.push("loop");
            }
            (Lang::Rust, "if_expression") => self.traits.push("conditional"),
            (Lang::Rust, "try_expression") => self.traits.push("error-handling"),
            (Lang::Rust, "unsafe_block") => self.traits.push("unsafe"),
            (Lang::Rust, "closure_expression") => self.traits.push("closure"),
            (Lang::Rust, "macro_invocation") => self.traits.push("macro-call"),
            (Lang::Rust, "call_expression") => {
                if let Some(func) = node.child_by_field_name("function") {
                    if let Ok(text) = func.utf8_text(self.source) {
                        if text.ends_with("unwrap") || text.ends_with("expect") {
                            self.traits.push("unwrap");
                        }
                    }
                }
            }
            (Lang::Rust, "return_expression") => self.traits.push("early-return"),
            // C traits
            (Lang::C, "switch_statement") => self.traits.push("match"),
            (Lang::C, "for_statement") | (Lang::C, "while_statement") | (Lang::C, "do_statement") => {
                self.traits.push("loop");
            }
            (Lang::C, "if_statement") => self.traits.push("conditional"),
            (Lang::C, "goto_statement") => self.traits.push("goto"),
            (Lang::C, "return_statement") => self.traits.push("early-return"),
            (Lang::C, "call_expression") => {
                if let Some(func) = node.child_by_field_name("function") {
                    if let Ok(text) = func.utf8_text(self.source) {
                        if text == "malloc" || text == "calloc" || text == "realloc" || text == "kmalloc" {
                            self.traits.push("alloc");
                        }
                        if text == "free" || text == "kfree" {
                            self.traits.push("free");
                        }
                    }
                }
            }
            (Lang::C, "pointer_expression") => self.traits.push("pointer-deref"),
            _ => {}
        }
        for i in 0..node.child_count() {
            self.walk(&node.child(i).unwrap());
        }
    }
}

// === Experiments ===

fn experiment_mode_comparison(fns: &[EncodedFn]) {
    println!("=== Experiment 1: Strip vs Embed mode ===\n");

    let mut agree = 0;
    let mut total = 0;
    let mut embed_sims = Vec::new();
    let mut strip_sims = Vec::new();

    let sample_size = fns.len().min(300);
    for i in 0..sample_size {
        let best_embed = nearest_neighbor(i, fns, |f| &f.vec_embed);
        let best_strip = nearest_neighbor(i, fns, |f| &f.vec_strip);
        if best_embed.0 == best_strip.0 {
            agree += 1;
        }
        embed_sims.push(best_embed.1);
        strip_sims.push(best_strip.1);
        total += 1;
    }

    let avg_embed: f64 = embed_sims.iter().sum::<f64>() / embed_sims.len() as f64;
    let avg_strip: f64 = strip_sims.iter().sum::<f64>() / strip_sims.len() as f64;

    println!("  NN agreement: {agree}/{total} ({:.1}%)", 100.0 * agree as f64 / total as f64);
    println!("  Avg NN similarity — embed: {avg_embed:.4}, strip: {avg_strip:.4}");
    println!();
}

fn experiment_discrimination(fns: &[EncodedFn]) {
    println!("=== Experiment 2: Same-file vs cross-file discrimination ===\n");

    let sample_size = fns.len().min(300);
    let mut same_e = Vec::new();
    let mut diff_e = Vec::new();
    let mut same_s = Vec::new();
    let mut diff_s = Vec::new();

    for i in 0..sample_size {
        for j in (i + 1)..sample_size {
            let se = fns[i].vec_embed.cosine_similarity(&fns[j].vec_embed);
            let ss = fns[i].vec_strip.cosine_similarity(&fns[j].vec_strip);
            if fns[i].file == fns[j].file {
                same_e.push(se); same_s.push(ss);
            } else {
                diff_e.push(se); diff_s.push(ss);
            }
        }
    }

    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    println!("  {:>22} {:>10} {:>10}", "", "embed", "strip");
    println!("  {:>22} {:>10.4} {:>10.4}", "same-file mean", mean(&same_e), mean(&same_s));
    println!("  {:>22} {:>10.4} {:>10.4}", "diff-file mean", mean(&diff_e), mean(&diff_s));
    println!("  {:>22} {:>10.4} {:>10.4}", "separation", mean(&same_e) - mean(&diff_e), mean(&same_s) - mean(&diff_s));
    println!();
}

fn experiment_structural_traits(fns: &[EncodedFn]) {
    println!("=== Experiment 3: Structural trait clustering ===\n");
    println!("Do functions sharing structural traits have higher HDC similarity?\n");

    let all_traits: Vec<&str> = vec![
        "match", "loop", "conditional", "error-handling", "unsafe",
        "closure", "early-return", "macro-call", "goto", "alloc",
    ];

    println!("  {:>16} {:>5} {:>11} {:>11} {:>11} {:>11}", "trait", "n",
        "intra(strip)", "inter(strip)", "intra(embed)", "inter(embed)");

    for tr in &all_traits {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i)
            .collect();
        if group.len() < 5 { continue; }
        let others: Vec<usize> = (0..fns.len()).filter(|i| !group.contains(i)).collect();

        let intra_s = avg_pairwise_sim(&group, fns, |f| &f.vec_strip, 1000);
        let inter_s = avg_cross_sim(&group, &others, fns, |f| &f.vec_strip, 1000);
        let intra_e = avg_pairwise_sim(&group, fns, |f| &f.vec_embed, 1000);
        let inter_e = avg_cross_sim(&group, &others, fns, |f| &f.vec_embed, 1000);

        let s_marker = if intra_s > inter_s { "+" } else { "-" };
        let e_marker = if intra_e > inter_e { "+" } else { "-" };

        println!("  {:>16} {:>5} {:>10.4}{} {:>10.4}  {:>10.4}{} {:>10.4}",
            tr, group.len(), intra_s, s_marker, inter_s, intra_e, e_marker, inter_e);
    }

    // Also test compound traits
    println!();
    println!("  Compound traits:");
    let compounds: Vec<(&str, Box<dyn Fn(&EncodedFn) -> bool>)> = vec![
        ("loop+conditional", Box::new(|f: &EncodedFn| f.traits.contains(&"loop") && f.traits.contains(&"conditional"))),
        ("match+early-ret", Box::new(|f: &EncodedFn| f.traits.contains(&"match") && f.traits.contains(&"early-return"))),
        ("error-handling+?", Box::new(|f: &EncodedFn| f.traits.contains(&"error-handling") && f.traits.contains(&"early-return"))),
        ("loop+unsafe", Box::new(|f: &EncodedFn| f.traits.contains(&"loop") && f.traits.contains(&"unsafe"))),
        ("pure (no traits)", Box::new(|f: &EncodedFn| f.traits.is_empty())),
    ];

    for (label, pred) in &compounds {
        let group: Vec<usize> = fns.iter().enumerate().filter(|(_, f)| pred(f)).map(|(i, _)| i).collect();
        if group.len() < 5 { continue; }
        let others: Vec<usize> = (0..fns.len()).filter(|i| !group.contains(i)).collect();

        let intra_s = avg_pairwise_sim(&group, fns, |f| &f.vec_strip, 1000);
        let inter_s = avg_cross_sim(&group, &others, fns, |f| &f.vec_strip, 1000);

        let marker = if intra_s > inter_s { "+" } else { "-" };
        println!("  {:>16} {:>5} {:>10.4}{} {:>10.4}", label, group.len(), intra_s, marker, inter_s);
    }
    println!();
}

fn experiment_precision_recall(fns: &[EncodedFn]) {
    println!("=== Experiment 4: Precision@k for structural trait retrieval ===\n");
    println!("For each function with trait T, how many of its top-k neighbors also have T?\n");

    let traits_to_test: Vec<&str> = vec!["match", "loop", "conditional", "error-handling", "unsafe", "closure"];

    println!("  {:>16} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "trait", "n", "P@1(s)", "P@5(s)", "P@10(s)", "P@1(e)", "P@5(e)", "P@10(e)");

    for tr in &traits_to_test {
        let group: Vec<usize> = fns.iter().enumerate()
            .filter(|(_, f)| f.traits.contains(tr))
            .map(|(i, _)| i)
            .collect();
        if group.len() < 10 { continue; }

        let base_rate = group.len() as f64 / fns.len() as f64;

        let mut p1_s = 0.0; let mut p5_s = 0.0; let mut p10_s = 0.0;
        let mut p1_e = 0.0; let mut p5_e = 0.0; let mut p10_e = 0.0;
        let sample: Vec<usize> = group.iter().copied().take(100).collect();

        for &qi in &sample {
            let ranked_s = top_k_neighbors(qi, fns, |f| &f.vec_strip, 10);
            let ranked_e = top_k_neighbors(qi, fns, |f| &f.vec_embed, 10);

            let hits_s: Vec<bool> = ranked_s.iter().map(|&(i, _)| fns[i].traits.contains(tr)).collect();
            let hits_e: Vec<bool> = ranked_e.iter().map(|&(i, _)| fns[i].traits.contains(tr)).collect();

            p1_s += hits_s[0] as u32 as f64;
            p5_s += hits_s[..5].iter().filter(|&&h| h).count() as f64 / 5.0;
            p10_s += hits_s.iter().filter(|&&h| h).count() as f64 / 10.0;

            p1_e += hits_e[0] as u32 as f64;
            p5_e += hits_e[..5].iter().filter(|&&h| h).count() as f64 / 5.0;
            p10_e += hits_e.iter().filter(|&&h| h).count() as f64 / 10.0;
        }

        let n = sample.len() as f64;
        println!("  {:>16} {:>5} {:>8.3} {:>8.3} {:>8.3}  {:>8.3} {:>8.3} {:>8.3}",
            tr, group.len(),
            p1_s / n, p5_s / n, p10_s / n,
            p1_e / n, p5_e / n, p10_e / n);
        println!("  {:>16} {:>5} (base rate: {:.3})", "", "", base_rate);
    }
    println!();
}

fn experiment_query_by_example(fns: &[EncodedFn]) {
    println!("=== Experiment 5: Query by example ===\n");

    let queries: Vec<usize> = if fns.len() > 400 {
        vec![0, fns.len() / 4, fns.len() / 2, 3 * fns.len() / 4]
    } else if fns.len() > 50 {
        vec![0, fns.len() / 3, 2 * fns.len() / 3]
    } else if !fns.is_empty() {
        vec![0]
    } else {
        return;
    };

    for qi in queries {
        let q = &fns[qi];
        let traits_str = if q.traits.is_empty() { "none".to_string() } else { q.traits.join(", ") };
        println!("Query: {}::{} ({} lines, traits: {})",
            short_path(&q.file), q.name, q.line_count, traits_str);

        let ranked_e = top_k_neighbors(qi, fns, |f| &f.vec_embed, 5);
        let ranked_s = top_k_neighbors(qi, fns, |f| &f.vec_strip, 5);

        println!("  {:42} {}", "Top 5 (embed):", "Top 5 (strip):");
        for k in 0..5 {
            let (ie, se) = ranked_e[k];
            let (is_, ss) = ranked_s[k];
            let te = if fns[ie].traits.is_empty() { String::new() } else { format!(" [{}]", fns[ie].traits.join(",")) };
            let ts = if fns[is_].traits.is_empty() { String::new() } else { format!(" [{}]", fns[is_].traits.join(",")) };
            let left = format!("{se:.3} {}::{}{te}", short_path(&fns[ie].file), fns[ie].name);
            let right = format!("{ss:.3} {}::{}{ts}", short_path(&fns[is_].file), fns[is_].name);
            println!("  {left:42} {right}");
        }
        println!();
    }
}

// === Helpers ===

fn nearest_neighbor(idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HdcVec) -> (usize, f64) {
    fns.iter().enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(i, f)| (i, get_vec(&fns[idx]).cosine_similarity(get_vec(f))))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap()
}

fn top_k_neighbors(idx: usize, fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HdcVec, k: usize) -> Vec<(usize, f64)> {
    let mut ranked: Vec<(usize, f64)> = fns.iter().enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(i, f)| (i, get_vec(&fns[idx]).cosine_similarity(get_vec(f))))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    ranked.truncate(k);
    ranked
}

fn avg_pairwise_sim(
    indices: &[usize], fns: &[EncodedFn], get_vec: impl Fn(&EncodedFn) -> &HdcVec, max_pairs: usize,
) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for (ii, &i) in indices.iter().enumerate() {
        for &j in &indices[ii + 1..] {
            sum += get_vec(&fns[i]).cosine_similarity(get_vec(&fns[j]));
            count += 1;
            if count >= max_pairs { return sum / count as f64; }
        }
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
}

fn avg_cross_sim(
    group_a: &[usize], group_b: &[usize], fns: &[EncodedFn],
    get_vec: impl Fn(&EncodedFn) -> &HdcVec, max_pairs: usize,
) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for &i in group_a {
        for &j in group_b {
            sum += get_vec(&fns[i]).cosine_similarity(get_vec(&fns[j]));
            count += 1;
            if count >= max_pairs { return sum / count as f64; }
        }
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
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
    if node.id() == id {
        return Some(*node);
    }
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
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

fn short_path(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, f)| f).unwrap_or(path)
}
