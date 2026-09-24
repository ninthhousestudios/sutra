//! Content → resolved `(file_id, target_id)` import edges for Rust and Dart.
//!
//! The edit-time guard and the diff review both derive edges from file content
//! rather than the index, and both feed the result to edge constraints and
//! max_fan_in attribution. They must resolve through this one extractor, or the
//! guard predicts a different outcome than the review reports (sutra/460).

use std::collections::HashMap;
use std::path::Path;

use crate::parser::ParseResult;

/// Whether `#[cfg(test)]`-scoped imports produce edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestImports {
    /// Keep test imports, matching the index's full edge set; per-constraint
    /// `include_tests` is honoured downstream.
    Keep,
    /// Drop test imports: the guard's policy, since an edit-time deny on test
    /// wiring is the failure mode of sutra/290.
    Drop,
}

/// Resolve the imports of a parsed Rust or Dart file against `path_ids`
/// (workspace-relative path → file id). Self-edges are omitted.
///
/// Returns `None` for any other language: content-based resolution only exists
/// for these two, and callers choose their own fallback.
pub fn content_import_edges(
    project_root: &Path,
    rel_path: &str,
    file_id: i64,
    language: &str,
    result: &ParseResult,
    path_ids: &HashMap<&str, i64>,
    tests: TestImports,
) -> Option<Vec<(i64, i64)>> {
    let imports = result
        .imports
        .iter()
        .filter(|i| tests == TestImports::Keep || !i.is_test);
    let mut edges = Vec::new();
    match language {
        "rust" => {
            let layout = crate::rust_imports::parse_workspace_layout(project_root);
            for import in imports {
                let resolved = match crate::rust_imports::normalize_to_crate_segments(
                    &import.raw_path,
                    rel_path,
                    &layout,
                ) {
                    Some(r) if !r.segments.is_empty() => r,
                    _ => continue,
                };
                if let Some(target_id) = crate::rust_imports::resolve_segments(
                    &resolved.segments,
                    path_ids,
                    &resolved.src_prefix,
                ) && target_id != file_id
                {
                    edges.push((file_id, target_id));
                }
            }
        }
        "dart" => {
            let mut pkg_map = None;
            for import in imports {
                let resolved = if import.raw_path.starts_with("package:") {
                    let map = pkg_map.get_or_insert_with(|| {
                        crate::dart_packages::DartPackageMap::build(project_root)
                    });
                    crate::dart_packages::resolve_package_uri(&import.raw_path, map)
                } else if import.raw_path.ends_with(".dart")
                    && !import.raw_path.starts_with("dart:")
                {
                    crate::dart_packages::resolve_relative_to(&import.raw_path, rel_path)
                } else {
                    None
                };
                if let Some(path) = resolved
                    && let Some(&target_id) = path_ids.get(path.as_str())
                    && target_id != file_id
                {
                    edges.push((file_id, target_id));
                }
            }
        }
        _ => return None,
    }
    Some(edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str, language: &str, rel_path: &str) -> ParseResult {
        let r = crate::parser::parse_file(content, language, rel_path).expect("parse");
        assert!(r.parsed_ok);
        r
    }

    #[test]
    fn dart_parent_relative_import_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: HashMap<&str, i64> = [
            ("lib/tabs/provider.dart", 1),
            ("lib/core/kernel.dart", 2),
            ("lib/tabs/sibling.dart", 3),
        ]
        .into_iter()
        .collect();
        let content = "import '../core/kernel.dart';\nimport 'sibling.dart';\nimport './provider.dart';\nimport 'dart:async';\n";
        let r = parse(content, "dart", "lib/tabs/provider.dart");
        let edges = content_import_edges(
            dir.path(),
            "lib/tabs/provider.dart",
            1,
            "dart",
            &r,
            &ids,
            TestImports::Drop,
        );
        assert_eq!(edges, Some(vec![(1, 2), (1, 3)]));
    }

    #[test]
    fn dart_relative_resolution_matches_indexer() {
        let ids: HashMap<i64, &str> = [(1, "lib/tabs/provider.dart")].into_iter().collect();
        for raw in [
            "../core/kernel.dart",
            "sibling.dart",
            "./a/b.dart",
            "../../../x.dart",
        ] {
            assert_eq!(
                crate::dart_packages::resolve_relative_import(raw, 1, &ids),
                crate::dart_packages::resolve_relative_to(raw, "lib/tabs/provider.dart"),
                "{raw}"
            );
        }
    }

    #[test]
    fn rust_test_import_policy_is_explicit() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"app\"\n")
            .expect("write Cargo.toml");
        let ids: HashMap<&str, i64> = [
            ("src/lib.rs", 1),
            ("src/render.rs", 2),
            ("src/fixtures.rs", 3),
        ]
        .into_iter()
        .collect();
        let content =
            "use crate::render;\n\n#[cfg(test)]\nmod tests {\n    use crate::fixtures;\n}\n";
        let r = parse(content, "rust", "src/lib.rs");
        let edges =
            |tests| content_import_edges(dir.path(), "src/lib.rs", 1, "rust", &r, &ids, tests);
        assert_eq!(edges(TestImports::Drop), Some(vec![(1, 2)]));
        assert_eq!(edges(TestImports::Keep), Some(vec![(1, 2), (1, 3)]));
    }

    #[test]
    fn other_languages_have_no_content_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = parse("import os\n", "python", "a.py");
        let ids = HashMap::new();
        assert_eq!(
            content_import_edges(dir.path(), "a.py", 1, "python", &r, &ids, TestImports::Keep),
            None
        );
    }
}
