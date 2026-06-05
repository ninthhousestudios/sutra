use std::collections::HashMap;
use std::path::Path;

use crate::db::Db;
use crate::git;
use crate::health::findings::{BiomarkerKind, HealthFinding};
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
            hrr_delta_threshold: 0.40,
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

fn extract_functions(
    tree: &tree_sitter::Tree,
    source: &[u8],
    fn_kinds: &[&str],
) -> Vec<FnNode> {
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
    registry: &LanguageRegistry,
    config: &ShapeChangeConfig,
) -> Vec<ShapeChange> {
    let existing = db.load_hrr_codebook().unwrap_or_default();
    let mut cb = Codebook::from_entries(existing);
    let mut results = Vec::new();

    for path in changed_paths {
        let ext = match Path::new(path).extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let adapter = match registry.adapter_for_extension(ext) {
            Some(a) => a,
            None => continue,
        };
        let fn_kinds = fn_kinds_for_language(adapter.language_id());
        if fn_kinds.is_empty() {
            continue;
        }

        let old_source = match git::git_file_content_at(workspace_root, base_revision, path) {
            Ok(Some(s)) => s,
            _ => continue,
        };

        let full_path = workspace_root.join(path);
        let new_source = match std::fs::read_to_string(&full_path) {
            Ok(s) => s,
            Err(_) => continue,
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

            let hrr_delta = 1.0 - old_vec.cosine_similarity(&new_vec);
            let text_delta = text_delta_ratio(&old_fn.source, &new_fn.source);
            let quadrant = classify(text_delta, hrr_delta, config);

            let symbol_id = file_id.and_then(|fid| {
                db.find_symbols_by_file(fid)
                    .ok()
                    .and_then(|syms| {
                        syms.into_iter()
                            .find(|s| s.short_name == new_fn.name)
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
    let node = tree
        .root_node()
        .descendant_for_point_range(start, end)?;
    Some(encoder::encode_subtree(&node, source, cb, false))
}

pub fn shape_changes_to_findings(changes: &[ShapeChange]) -> Vec<HealthFinding> {
    changes
        .iter()
        .filter(|c| c.quadrant == DiffQuadrant::SubtleStructural)
        .map(|c| HealthFinding {
            file_id: c.file_id.unwrap_or(0),
            symbol_id: c.symbol_id,
            biomarker_kind: BiomarkerKind::HrrShapeChange,
            severity: BiomarkerKind::HrrShapeChange.default_severity(),
            confidence: 1.0 - c.text_delta,
            provenance: "hrr_semantic_diff".into(),
            metric_value: c.hrr_delta,
            threshold: 0.40,
            detail: format!(
                "{}: text changed {:.0}% but structural shape changed {:.0}%",
                c.symbol_name,
                c.text_delta * 100.0,
                c.hrr_delta * 100.0,
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quadrant_classification() {
        let config = ShapeChangeConfig::default();

        assert_eq!(classify(0.05, 0.10, &config), DiffQuadrant::Trivial);
        assert_eq!(classify(0.05, 0.60, &config), DiffQuadrant::SubtleStructural);
        assert_eq!(classify(0.50, 0.10, &config), DiffQuadrant::SafeRefactoring);
        assert_eq!(classify(0.50, 0.60, &config), DiffQuadrant::MajorRewrite);
    }

    #[test]
    fn test_quadrant_boundaries() {
        let config = ShapeChangeConfig::default();

        // Exactly at thresholds: text_delta=0.15 is "small", hrr_delta=0.40 is "large"
        assert_eq!(classify(0.15, 0.39, &config), DiffQuadrant::Trivial);
        assert_eq!(classify(0.15, 0.40, &config), DiffQuadrant::SubtleStructural);
        assert_eq!(classify(0.16, 0.40, &config), DiffQuadrant::MajorRewrite);
        assert_eq!(classify(0.16, 0.39, &config), DiffQuadrant::SafeRefactoring);
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
    fn test_shape_change_to_finding_filters_quadrant() {
        let changes = vec![
            ShapeChange {
                file_path: "a.rs".into(),
                symbol_name: "foo".into(),
                symbol_id: Some(1),
                file_id: Some(10),
                text_delta: 0.05,
                hrr_delta: 0.70,
                quadrant: DiffQuadrant::SubtleStructural,
            },
            ShapeChange {
                file_path: "b.rs".into(),
                symbol_name: "bar".into(),
                symbol_id: Some(2),
                file_id: Some(20),
                text_delta: 0.50,
                hrr_delta: 0.70,
                quadrant: DiffQuadrant::MajorRewrite,
            },
        ];
        let findings = shape_changes_to_findings(&changes);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file_id, 10);
        assert_eq!(findings[0].biomarker_kind, BiomarkerKind::HrrShapeChange);
        assert!((findings[0].metric_value - 0.70).abs() < 1e-9);
    }
}
