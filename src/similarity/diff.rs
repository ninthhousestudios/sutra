use std::collections::HashMap;
use std::path::Path;

use tracing::debug;

use crate::db::Db;
use crate::git;
use crate::parser::adapter::LanguageRegistry;
use crate::similarity::{codebook::Codebook, encoder, hrr::HrrVec};

const RUST_FN_KINDS: &[&str] = &["function_item"];
const DART_FN_KINDS: &[&str] = &["function_declaration", "method_declaration"];

pub struct ShapeChangeConfig {
    pub text_delta_threshold: f64,
    pub hrr_delta_threshold: f64,
}

impl Default for ShapeChangeConfig {
    fn default() -> Self {
        Self {
            text_delta_threshold: 0.15,
            hrr_delta_threshold: 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffQuadrant {
    Trivial,
    SubtleStructural,
    SafeRefactoring,
    MajorRewrite,
}

impl DiffQuadrant {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::SubtleStructural => "subtle_structural",
            Self::SafeRefactoring => "safe_refactoring",
            Self::MajorRewrite => "major_rewrite",
        }
    }
}

fn classify(text_delta: f64, hrr_delta: f64, config: &ShapeChangeConfig) -> DiffQuadrant {
    let small_text = text_delta <= config.text_delta_threshold;
    let large_hrr = hrr_delta >= config.hrr_delta_threshold;
    match (small_text, large_hrr) {
        (true, false) => DiffQuadrant::Trivial,
        (true, true) => DiffQuadrant::SubtleStructural,
        (false, false) => DiffQuadrant::SafeRefactoring,
        (false, true) => DiffQuadrant::MajorRewrite,
    }
}

#[derive(Debug, Clone)]
pub struct ShapeChange {
    pub file_path: String,
    pub symbol_name: String,
    pub symbol_id: Option<i64>,
    pub file_id: Option<i64>,
    pub text_delta: f64,
    pub hrr_delta: f64,
    pub quadrant: DiffQuadrant,
}

fn text_delta_ratio(old: &str, new: &str) -> f64 {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let max_len = old_lines.len().max(new_lines.len());
    if max_len == 0 {
        return 0.0;
    }
    let mut diff_count = old_lines.len().abs_diff(new_lines.len());
    let common_len = old_lines.len().min(new_lines.len());
    for i in 0..common_len {
        if old_lines[i] != new_lines[i] {
            diff_count += 1;
        }
    }
    diff_count as f64 / max_len as f64
}

fn fn_kinds_for_language(lang_id: &str) -> &'static [&'static str] {
    match lang_id {
        "rust" => RUST_FN_KINDS,
        "dart" => DART_FN_KINDS,
        _ => &[],
    }
}

struct FnNode {
    name: String,
    source: String,
    ts_node_start: tree_sitter::Point,
    ts_node_end: tree_sitter::Point,
}

fn extract_functions(tree: &tree_sitter::Tree, source: &[u8], fn_kinds: &[&str]) -> Vec<FnNode> {
    let mut results = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if fn_kinds.contains(&node.kind())
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source)
            && let Ok(src) = node.utf8_text(source)
        {
            results.push(FnNode {
                name: name.to_string(),
                source: src.to_string(),
                ts_node_start: node.start_position(),
                ts_node_end: node.end_position(),
            });
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    results
}

pub fn detect_shape_changes(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    base_revision: &str,
    head_revision: Option<&str>,
    registry: &LanguageRegistry,
    config: &ShapeChangeConfig,
) -> Vec<ShapeChange> {
    let existing = db.load_hrr_codebook().unwrap_or_default();
    let mut cb = Codebook::from_entries(existing);
    let mut results = Vec::new();

    for path in changed_paths {
        let ext = match Path::new(path).extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => {
                debug!(path, "shape_diff: skip — no extension");
                continue;
            }
        };
        let adapter = match registry.adapter_for_extension(ext) {
            Some(a) => a,
            None => {
                debug!(path, ext, "shape_diff: skip — no adapter");
                continue;
            }
        };
        let fn_kinds = fn_kinds_for_language(adapter.language_id());
        if fn_kinds.is_empty() {
            debug!(
                path,
                lang = adapter.language_id(),
                "shape_diff: skip — no fn_kinds"
            );
            continue;
        }

        let old_source = match git::git_file_content_at(workspace_root, base_revision, path) {
            Ok(Some(s)) => s,
            Ok(None) => {
                debug!(path, "shape_diff: skip — new file");
                continue;
            }
            Err(e) => {
                debug!(path, err = %e, "shape_diff: skip — git error");
                continue;
            }
        };

        let new_source = match head_revision {
            Some(rev) => match git::git_file_content_at(workspace_root, rev, path) {
                Ok(Some(s)) => s,
                Ok(None) | Err(_) => continue,
            },
            None => match std::fs::read_to_string(workspace_root.join(path)) {
                Ok(s) => s,
                Err(_) => continue,
            },
        };

        let grammar = adapter.grammar();
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&grammar).is_err() {
            continue;
        }

        let old_tree = match parser.parse(&old_source, None) {
            Some(t) => t,
            None => continue,
        };
        let new_tree = match parser.parse(&new_source, None) {
            Some(t) => t,
            None => continue,
        };

        let old_fns = extract_functions(&old_tree, old_source.as_bytes(), fn_kinds);
        let new_fns = extract_functions(&new_tree, new_source.as_bytes(), fn_kinds);
        debug!(
            path,
            old_fn_count = old_fns.len(),
            new_fn_count = new_fns.len(),
            "shape_diff: parsed"
        );

        let old_map: HashMap<&str, &FnNode> =
            old_fns.iter().map(|f| (f.name.as_str(), f)).collect();

        let file_id = db.file_by_path(path).ok().flatten().map(|f| f.id);

        for new_fn in &new_fns {
            let old_fn = match old_map.get(new_fn.name.as_str()) {
                Some(f) => f,
                None => continue,
            };

            let old_strip = encode_fn_node(
                &old_tree,
                old_fn.ts_node_start,
                old_fn.ts_node_end,
                old_source.as_bytes(),
                &mut cb,
            );
            let new_strip = encode_fn_node(
                &new_tree,
                new_fn.ts_node_start,
                new_fn.ts_node_end,
                new_source.as_bytes(),
                &mut cb,
            );

            let (old_vec, new_vec) = match (old_strip, new_strip) {
                (Some(o), Some(n)) => (o, n),
                _ => continue,
            };

            let hrr_delta_raw = 1.0 - old_vec.cosine_similarity(&new_vec);
            let n_children =
                fn_body_child_count(&old_tree, old_fn.ts_node_start, old_fn.ts_node_end);
            let hrr_delta = hrr_delta_raw * n_children as f64;
            let text_delta = text_delta_ratio(&old_fn.source, &new_fn.source);
            let quadrant = classify(text_delta, hrr_delta, config);
            debug!(
                path,
                symbol = new_fn.name,
                hrr_delta_raw = format!("{hrr_delta_raw:.3}"),
                hrr_delta_norm = format!("{hrr_delta:.3}"),
                n_children,
                text_delta = format!("{text_delta:.3}"),
                quadrant = quadrant.as_str(),
                "shape_diff: function compared"
            );

            let symbol_id = file_id.and_then(|fid| {
                db.find_symbols_by_file(fid).ok().and_then(|syms| {
                    syms.into_iter()
                        .find(|s| *s.short_name == *new_fn.name)
                        .map(|s| s.id)
                })
            });

            results.push(ShapeChange {
                file_path: path.clone(),
                symbol_name: new_fn.name.clone(),
                symbol_id,
                file_id,
                text_delta,
                hrr_delta,
                quadrant,
            });
        }
    }
    results
}

fn encode_fn_node(
    tree: &tree_sitter::Tree,
    start: tree_sitter::Point,
    end: tree_sitter::Point,
    source: &[u8],
    cb: &mut Codebook,
) -> Option<HrrVec> {
    let node = tree.root_node().descendant_for_point_range(start, end)?;
    Some(encoder::encode_subtree(&node, source, cb, false))
}

fn fn_body_child_count(
    tree: &tree_sitter::Tree,
    start: tree_sitter::Point,
    end: tree_sitter::Point,
) -> usize {
    let node = match tree.root_node().descendant_for_point_range(start, end) {
        Some(n) => n,
        None => return 1,
    };
    let block = (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|c| c.kind() == "block" || c.kind() == "function_body");
    match block {
        Some(b) => (0..b.child_count())
            .filter_map(|i| b.child(i))
            .filter(|c| c.is_named())
            .count()
            .max(1),
        None => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quadrant_classification() {
        let config = ShapeChangeConfig::default();

        assert_eq!(classify(0.05, 0.10, &config), DiffQuadrant::Trivial);
        assert_eq!(
            classify(0.05, 0.60, &config),
            DiffQuadrant::SubtleStructural
        );
        assert_eq!(classify(0.50, 0.10, &config), DiffQuadrant::SafeRefactoring);
        assert_eq!(classify(0.50, 0.60, &config), DiffQuadrant::MajorRewrite);
    }

    #[test]
    fn test_quadrant_boundaries() {
        let config = ShapeChangeConfig::default();

        // Exactly at thresholds: text_delta=0.15 is "small", hrr_delta=0.15 is "large"
        assert_eq!(classify(0.15, 0.14, &config), DiffQuadrant::Trivial);
        assert_eq!(
            classify(0.15, 0.15, &config),
            DiffQuadrant::SubtleStructural
        );
        assert_eq!(classify(0.16, 0.15, &config), DiffQuadrant::MajorRewrite);
        assert_eq!(classify(0.16, 0.14, &config), DiffQuadrant::SafeRefactoring);
    }

    #[test]
    fn test_text_delta_identical() {
        assert_eq!(text_delta_ratio("fn foo() {}\n", "fn foo() {}\n"), 0.0);
    }

    #[test]
    fn test_text_delta_completely_different() {
        assert_eq!(text_delta_ratio("aaa\nbbb\n", "ccc\nddd\n"), 1.0);
    }

    #[test]
    fn test_text_delta_one_line_changed() {
        let old = "line1\nline2\nline3\nline4\n";
        let new = "line1\nchanged\nline3\nline4\n";
        assert!((text_delta_ratio(old, new) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn test_text_delta_added_lines() {
        let old = "line1\nline2\n";
        let new = "line1\nline2\nline3\nline4\n";
        // max_len=4, diff=2 (added lines) + 0 (common)
        assert!((text_delta_ratio(old, new) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_text_delta_empty() {
        assert_eq!(text_delta_ratio("", ""), 0.0);
    }

    #[test]
    fn test_extract_functions_rust() {
        let source = r#"
fn foo(x: i32) -> i32 {
    x + 1
}

fn bar() {
    println!("hello");
}
"#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let fns = extract_functions(&tree, source.as_bytes(), RUST_FN_KINDS);
        let mut names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["bar", "foo"]);
    }

    #[test]
    fn test_hrr_delta_detects_structural_change() {
        let old_source = r#"
fn example(x: i32) -> i32 {
    if x > 0 {
        x + 1
    } else {
        0
    }
}
"#;
        let new_source = r#"
fn example(x: i32) -> i32 {
    match x > 0 {
        true => x + 1,
        false => 0,
    }
}
"#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let old_tree = parser.parse(old_source, None).unwrap();
        let new_tree = parser.parse(new_source, None).unwrap();

        let old_fns = extract_functions(&old_tree, old_source.as_bytes(), RUST_FN_KINDS);
        let new_fns = extract_functions(&new_tree, new_source.as_bytes(), RUST_FN_KINDS);
        assert_eq!(old_fns.len(), 1);
        assert_eq!(new_fns.len(), 1);
        assert_eq!(old_fns[0].name, "example");

        let mut cb = Codebook::from_entries(std::collections::HashMap::new());
        let old_vec = encode_fn_node(
            &old_tree,
            old_fns[0].ts_node_start,
            old_fns[0].ts_node_end,
            old_source.as_bytes(),
            &mut cb,
        )
        .unwrap();
        let new_vec = encode_fn_node(
            &new_tree,
            new_fns[0].ts_node_start,
            new_fns[0].ts_node_end,
            new_source.as_bytes(),
            &mut cb,
        )
        .unwrap();

        let hrr_delta = 1.0 - old_vec.cosine_similarity(&new_vec);
        let text_delta = text_delta_ratio(&old_fns[0].source, &new_fns[0].source);

        // Structural change (if→match) should produce measurable HRR delta
        assert!(hrr_delta > 0.0, "HRR delta should be > 0, got {hrr_delta}");
        // Text change is moderate
        eprintln!("hrr_delta={hrr_delta:.3}, text_delta={text_delta:.3}");
    }

    #[test]
    fn test_subtle_structural_detection() {
        // 15-line function; change 1 line from simple expression to
        // deeply nested block — text_delta should be low, hrr_delta high
        let old = r#"
fn process(data: &[i32]) -> i32 {
    let mut total = 0;
    let mut count = 0;
    let base = 10;
    let factor = 2;
    let offset = 5;
    let limit = 100;
    let step = 1;
    total += base * factor;
    total += offset;
    count += step;
    if count > limit { total = limit; }
    total
}
"#;
        let new = r#"
fn process(data: &[i32]) -> i32 {
    let mut total = 0;
    let mut count = 0;
    let base = 10;
    let factor = 2;
    let offset = 5;
    let limit = 100;
    let step = 1;
    total += base * factor;
    total += offset;
    count += step;
    if count > limit { for i in data { if *i > 0 { total += *i; } } }
    total
}
"#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let old_tree = parser.parse(old, None).unwrap();
        let new_tree = parser.parse(new, None).unwrap();

        let old_fns = extract_functions(&old_tree, old.as_bytes(), RUST_FN_KINDS);
        let new_fns = extract_functions(&new_tree, new.as_bytes(), RUST_FN_KINDS);
        assert_eq!(old_fns.len(), 1);
        assert_eq!(new_fns.len(), 1);

        let mut cb = Codebook::from_entries(std::collections::HashMap::new());
        let old_vec = encode_fn_node(
            &old_tree,
            old_fns[0].ts_node_start,
            old_fns[0].ts_node_end,
            old.as_bytes(),
            &mut cb,
        )
        .unwrap();
        let new_vec = encode_fn_node(
            &new_tree,
            new_fns[0].ts_node_start,
            new_fns[0].ts_node_end,
            new.as_bytes(),
            &mut cb,
        )
        .unwrap();

        let hrr_delta_raw = 1.0 - old_vec.cosine_similarity(&new_vec);
        let n_children =
            count_body_children(&old_tree, old_fns[0].ts_node_start, old_fns[0].ts_node_end);
        let hrr_delta = hrr_delta_raw * n_children as f64;
        let text_delta = text_delta_ratio(&old_fns[0].source, &new_fns[0].source);
        let config = ShapeChangeConfig::default();
        let quadrant = classify(text_delta, hrr_delta, &config);

        eprintln!(
            "subtle test: hrr_raw={hrr_delta_raw:.3}, hrr_norm={hrr_delta:.3}, n={n_children}, text_delta={text_delta:.3}, quadrant={:?}",
            quadrant
        );
        assert!(
            text_delta <= config.text_delta_threshold,
            "text_delta {text_delta:.3} should be ≤ {}",
            config.text_delta_threshold
        );
        assert_eq!(
            quadrant,
            DiffQuadrant::SubtleStructural,
            "with size normalization, nested-block change should reach SubtleStructural"
        );
    }

    /// Generate a function with `n` statements; returns (old_source, new_source)
    /// where old has a simple assignment and new has a nested for+if block in its place.
    fn make_fn_pair(num_statements: usize) -> (String, String) {
        assert!(num_statements >= 4);
        let mut old_lines = vec!["fn process(data: &[i32]) -> i32 {".to_string()];
        let mut new_lines = old_lines.clone();

        old_lines.push("    let mut total = 0;".into());
        new_lines.push("    let mut total = 0;".into());

        for i in 0..(num_statements - 3) {
            let line = format!("    let v{i} = {i};");
            old_lines.push(line.clone());
            new_lines.push(line);
        }

        old_lines.push("    total += 1;".into());
        new_lines.push("    for i in data { if *i > 0 { total += *i; } }".into());

        old_lines.push("    total".into());
        new_lines.push("    total".into());

        old_lines.push("}".into());
        new_lines.push("}".into());

        (old_lines.join("\n"), new_lines.join("\n"))
    }

    fn count_body_children(
        tree: &tree_sitter::Tree,
        fn_node_start: tree_sitter::Point,
        fn_node_end: tree_sitter::Point,
    ) -> usize {
        let node = tree
            .root_node()
            .descendant_for_point_range(fn_node_start, fn_node_end)
            .unwrap();
        let block = (0..node.child_count())
            .filter_map(|i| node.child(i))
            .find(|c| c.kind() == "block")
            .unwrap();
        (0..block.child_count())
            .filter_map(|i| block.child(i))
            .filter(|c| c.is_named())
            .count()
    }

    #[test]
    fn test_hrr_delta_normalization_candidates() {
        let sizes = [4, 7, 10, 15, 25, 40];

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        eprintln!(
            "\n=== HRR delta vs function size — single structural change (assignment → for+if) ===\n"
        );
        eprintln!(
            "{:<6} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10}",
            "stmts", "hrr_raw", "text_d", "n_kids", "*sqrt(n)", "*n", "*ln(n)"
        );
        eprintln!("{}", "-".repeat(66));

        let mut raw_deltas = Vec::new();

        for &size in &sizes {
            let (old_src, new_src) = make_fn_pair(size);

            let old_tree = parser.parse(&old_src, None).unwrap();
            let new_tree = parser.parse(&new_src, None).unwrap();

            let old_fns = extract_functions(&old_tree, old_src.as_bytes(), RUST_FN_KINDS);
            let new_fns = extract_functions(&new_tree, new_src.as_bytes(), RUST_FN_KINDS);
            assert_eq!(old_fns.len(), 1, "size={size}");
            assert_eq!(new_fns.len(), 1, "size={size}");

            let mut cb = Codebook::from_entries(std::collections::HashMap::new());
            let old_vec = encode_fn_node(
                &old_tree,
                old_fns[0].ts_node_start,
                old_fns[0].ts_node_end,
                old_src.as_bytes(),
                &mut cb,
            )
            .unwrap();
            let new_vec = encode_fn_node(
                &new_tree,
                new_fns[0].ts_node_start,
                new_fns[0].ts_node_end,
                new_src.as_bytes(),
                &mut cb,
            )
            .unwrap();

            let hrr_delta = 1.0 - old_vec.cosine_similarity(&new_vec);
            let text_delta = text_delta_ratio(&old_fns[0].source, &new_fns[0].source);
            let n = count_body_children(&old_tree, old_fns[0].ts_node_start, old_fns[0].ts_node_end)
                as f64;

            raw_deltas.push((size, hrr_delta, text_delta, n));

            eprintln!(
                "{:<6} {:<8.4} {:<8.4} {:<8} {:<10.4} {:<10.4} {:<10.4}",
                size,
                hrr_delta,
                text_delta,
                n as usize,
                hrr_delta * n.sqrt(),
                hrr_delta * n,
                hrr_delta * n.ln(),
            );
        }

        // Compute coefficient of variation for each normalization to find most consistent
        for (label, norm_fn) in [
            ("raw", (|d: f64, _n: f64| d) as fn(f64, f64) -> f64),
            ("*sqrt(n)", |d, n| d * n.sqrt()),
            ("*n", |d, n| d * n),
            ("*ln(n)", |d, n| d * n.ln()),
        ] {
            let vals: Vec<f64> = raw_deltas
                .iter()
                .map(|&(_, d, _, n)| norm_fn(d, n))
                .collect();
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
            let cv = variance.sqrt() / mean;
            eprintln!("  {label:<10} mean={mean:.4}  CV={cv:.4}  (lower CV = more consistent)");
        }

        // Also test a second change type: if→match
        eprintln!("\n=== HRR delta vs function size — if→match structural change ===\n");
        eprintln!(
            "{:<6} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10}",
            "stmts", "hrr_raw", "text_d", "n_kids", "*sqrt(n)", "*n", "*ln(n)"
        );
        eprintln!("{}", "-".repeat(66));

        let mut raw_deltas2 = Vec::new();

        for &size in &sizes {
            let (old_src, new_src) = make_fn_pair_if_to_match(size);

            let old_tree = parser.parse(&old_src, None).unwrap();
            let new_tree = parser.parse(&new_src, None).unwrap();

            let old_fns = extract_functions(&old_tree, old_src.as_bytes(), RUST_FN_KINDS);
            let new_fns = extract_functions(&new_tree, new_src.as_bytes(), RUST_FN_KINDS);
            assert_eq!(old_fns.len(), 1, "if→match size={size}");
            assert_eq!(new_fns.len(), 1, "if→match size={size}");

            let mut cb = Codebook::from_entries(std::collections::HashMap::new());
            let old_vec = encode_fn_node(
                &old_tree,
                old_fns[0].ts_node_start,
                old_fns[0].ts_node_end,
                old_src.as_bytes(),
                &mut cb,
            )
            .unwrap();
            let new_vec = encode_fn_node(
                &new_tree,
                new_fns[0].ts_node_start,
                new_fns[0].ts_node_end,
                new_src.as_bytes(),
                &mut cb,
            )
            .unwrap();

            let hrr_delta = 1.0 - old_vec.cosine_similarity(&new_vec);
            let text_delta = text_delta_ratio(&old_fns[0].source, &new_fns[0].source);
            let n = count_body_children(&old_tree, old_fns[0].ts_node_start, old_fns[0].ts_node_end)
                as f64;

            raw_deltas2.push((size, hrr_delta, text_delta, n));

            eprintln!(
                "{:<6} {:<8.4} {:<8.4} {:<8} {:<10.4} {:<10.4} {:<10.4}",
                size,
                hrr_delta,
                text_delta,
                n as usize,
                hrr_delta * n.sqrt(),
                hrr_delta * n,
                hrr_delta * n.ln(),
            );
        }

        for (label, norm_fn) in [
            ("raw", (|d: f64, _n: f64| d) as fn(f64, f64) -> f64),
            ("*sqrt(n)", |d, n| d * n.sqrt()),
            ("*n", |d, n| d * n),
            ("*ln(n)", |d, n| d * n.ln()),
        ] {
            let vals: Vec<f64> = raw_deltas2
                .iter()
                .map(|&(_, d, _, n)| norm_fn(d, n))
                .collect();
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
            let cv = variance.sqrt() / mean;
            eprintln!("  {label:<10} mean={mean:.4}  CV={cv:.4}  (lower CV = more consistent)");
        }

        // Trivial changes: value-only edit (1 → 42), no structural change
        eprintln!(
            "\n=== HRR delta vs function size — trivial change (value edit, same structure) ===\n"
        );
        eprintln!(
            "{:<6} {:<8} {:<8} {:<8} {:<10} {:<10} {:<10}",
            "stmts", "hrr_raw", "text_d", "n_kids", "*sqrt(n)", "*n", "*ln(n)"
        );
        eprintln!("{}", "-".repeat(66));

        let mut raw_deltas_trivial = Vec::new();

        for &size in &sizes {
            let (old_src, new_src) = make_fn_pair_rename(size);

            let old_tree = parser.parse(&old_src, None).unwrap();
            let new_tree = parser.parse(&new_src, None).unwrap();

            let old_fns = extract_functions(&old_tree, old_src.as_bytes(), RUST_FN_KINDS);
            let new_fns = extract_functions(&new_tree, new_src.as_bytes(), RUST_FN_KINDS);
            assert_eq!(old_fns.len(), 1, "trivial size={size}");
            assert_eq!(new_fns.len(), 1, "trivial size={size}");

            let mut cb = Codebook::from_entries(std::collections::HashMap::new());
            let old_vec = encode_fn_node(
                &old_tree,
                old_fns[0].ts_node_start,
                old_fns[0].ts_node_end,
                old_src.as_bytes(),
                &mut cb,
            )
            .unwrap();
            let new_vec = encode_fn_node(
                &new_tree,
                new_fns[0].ts_node_start,
                new_fns[0].ts_node_end,
                new_src.as_bytes(),
                &mut cb,
            )
            .unwrap();

            let hrr_delta = 1.0 - old_vec.cosine_similarity(&new_vec);
            let text_delta = text_delta_ratio(&old_fns[0].source, &new_fns[0].source);
            let n = count_body_children(&old_tree, old_fns[0].ts_node_start, old_fns[0].ts_node_end)
                as f64;

            raw_deltas_trivial.push((size, hrr_delta, text_delta, n));

            eprintln!(
                "{:<6} {:<8.4} {:<8.4} {:<8} {:<10.4} {:<10.4} {:<10.4}",
                size,
                hrr_delta,
                text_delta,
                n as usize,
                hrr_delta * n.sqrt(),
                hrr_delta * n,
                hrr_delta * n.ln(),
            );
        }

        for (label, norm_fn) in [
            ("raw", (|d: f64, _n: f64| d) as fn(f64, f64) -> f64),
            ("*sqrt(n)", |d, n| d * n.sqrt()),
            ("*n", |d, n| d * n),
            ("*ln(n)", |d, n| d * n.ln()),
        ] {
            let vals: Vec<f64> = raw_deltas_trivial
                .iter()
                .map(|&(_, d, _, n)| norm_fn(d, n))
                .collect();
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
            let cv = variance.sqrt() / mean;
            eprintln!("  {label:<10} mean={mean:.4}  CV={cv:.4}");
        }

        // Minor structural: add one let statement (should be low-signal)
        eprintln!(
            "\n=== HRR delta vs function size — minor structural (add one let binding) ===\n"
        );
        eprintln!(
            "{:<6} {:<8} {:<8} {:<8} {:<10}",
            "stmts", "hrr_raw", "text_d", "n_kids", "*n"
        );
        eprintln!("{}", "-".repeat(48));

        let mut raw_deltas_minor = Vec::new();

        for &size in &sizes {
            let (old_src, new_src) = make_fn_pair_add_let(size);

            let old_tree = parser.parse(&old_src, None).unwrap();
            let new_tree = parser.parse(&new_src, None).unwrap();

            let old_fns = extract_functions(&old_tree, old_src.as_bytes(), RUST_FN_KINDS);
            let new_fns = extract_functions(&new_tree, new_src.as_bytes(), RUST_FN_KINDS);

            let mut cb = Codebook::from_entries(std::collections::HashMap::new());
            let old_vec = encode_fn_node(
                &old_tree,
                old_fns[0].ts_node_start,
                old_fns[0].ts_node_end,
                old_src.as_bytes(),
                &mut cb,
            )
            .unwrap();
            let new_vec = encode_fn_node(
                &new_tree,
                new_fns[0].ts_node_start,
                new_fns[0].ts_node_end,
                new_src.as_bytes(),
                &mut cb,
            )
            .unwrap();

            let hrr_delta = 1.0 - old_vec.cosine_similarity(&new_vec);
            let text_delta = text_delta_ratio(&old_fns[0].source, &new_fns[0].source);
            let n = count_body_children(&old_tree, old_fns[0].ts_node_start, old_fns[0].ts_node_end)
                as f64;

            raw_deltas_minor.push((size, hrr_delta, text_delta, n));

            eprintln!(
                "{:<6} {:<8.4} {:<8.4} {:<8} {:<10.4}",
                size,
                hrr_delta,
                text_delta,
                n as usize,
                hrr_delta * n,
            );
        }

        let minor_mean = raw_deltas_minor
            .iter()
            .map(|&(_, d, _, n)| d * n)
            .sum::<f64>()
            / raw_deltas_minor.len() as f64;
        let minor_cv = {
            let vals: Vec<f64> = raw_deltas_minor.iter().map(|&(_, d, _, n)| d * n).collect();
            let var =
                vals.iter().map(|v| (v - minor_mean).powi(2)).sum::<f64>() / vals.len() as f64;
            var.sqrt() / minor_mean
        };
        eprintln!("  *n         mean={minor_mean:.4}  CV={minor_cv:.4}");

        // Separation check
        let structural_mean =
            raw_deltas.iter().map(|&(_, d, _, n)| d * n).sum::<f64>() / raw_deltas.len() as f64;
        let structural2_mean =
            raw_deltas2.iter().map(|&(_, d, _, n)| d * n).sum::<f64>() / raw_deltas2.len() as f64;
        eprintln!("\n=== Separation summary (using *n normalization) ===");
        eprintln!("  for+if structural: {structural_mean:.4}");
        eprintln!("  if→match struct:   {structural2_mean:.4}");
        eprintln!("  add-let minor:     {minor_mean:.4}");
        eprintln!("  value-only trivial: ~0.0000");
        eprintln!("  ---");
        eprintln!(
            "  structural / minor ratio: {:.1}x",
            structural_mean / minor_mean.max(1e-9)
        );

        // Sanity: raw HRR delta should decrease as function size grows
        for pair in raw_deltas.windows(2) {
            assert!(
                pair[0].1 >= pair[1].1,
                "raw hrr_delta should decrease with size: {}={:.4} vs {}={:.4}",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
    }

    /// Trivial change: rename a variable (no structural change)
    fn make_fn_pair_rename(num_statements: usize) -> (String, String) {
        assert!(num_statements >= 4);
        let mut old_lines = vec!["fn process(data: &[i32]) -> i32 {".to_string()];
        let mut new_lines = old_lines.clone();

        old_lines.push("    let mut total = 0;".into());
        new_lines.push("    let mut total = 0;".into());

        for i in 0..(num_statements - 3) {
            let line = format!("    let v{i} = {i};");
            old_lines.push(line.clone());
            new_lines.push(line);
        }

        // Rename: total → result (same structure)
        old_lines.push("    total += 1;".into());
        new_lines.push("    total += 42;".into());

        old_lines.push("    total".into());
        new_lines.push("    total".into());

        old_lines.push("}".into());
        new_lines.push("}".into());

        (old_lines.join("\n"), new_lines.join("\n"))
    }

    /// Minor structural change: add one extra let statement
    fn make_fn_pair_add_let(num_statements: usize) -> (String, String) {
        assert!(num_statements >= 4);
        let mut old_lines = vec!["fn process(data: &[i32]) -> i32 {".to_string()];
        let mut new_lines = old_lines.clone();

        old_lines.push("    let mut total = 0;".into());
        new_lines.push("    let mut total = 0;".into());

        for i in 0..(num_statements - 3) {
            let line = format!("    let v{i} = {i};");
            old_lines.push(line.clone());
            new_lines.push(line);
        }

        old_lines.push("    total += 1;".into());
        // Add an extra let + keep the assignment
        new_lines.push("    let extra = 99;".into());
        new_lines.push("    total += extra;".into());

        old_lines.push("    total".into());
        new_lines.push("    total".into());

        old_lines.push("}".into());
        new_lines.push("}".into());

        (old_lines.join("\n"), new_lines.join("\n"))
    }

    fn make_fn_pair_if_to_match(num_statements: usize) -> (String, String) {
        assert!(num_statements >= 4);
        let mut old_lines = vec!["fn process(data: &[i32]) -> i32 {".to_string()];
        let mut new_lines = old_lines.clone();

        old_lines.push("    let mut total = 0;".into());
        new_lines.push("    let mut total = 0;".into());

        for i in 0..(num_statements - 3) {
            let line = format!("    let v{i} = {i};");
            old_lines.push(line.clone());
            new_lines.push(line);
        }

        // if→match: same semantic, different structure
        old_lines.push("    if total > 0 { total += 1; } else { total -= 1; }".into());
        new_lines.push(
            "    match total > 0 { true => { total += 1; } false => { total -= 1; } }".into(),
        );

        old_lines.push("    total".into());
        new_lines.push("    total".into());

        old_lines.push("}".into());
        new_lines.push("}".into());

        (old_lines.join("\n"), new_lines.join("\n"))
    }
}
