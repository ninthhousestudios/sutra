pub mod check;
mod engine;
pub mod external;
pub mod finding;
pub mod patterns;
mod resolver;
mod worker;

use std::collections::HashMap;

use glob::{MatchOptions, Pattern};

pub use engine::DdEngine;
pub use finding::{ConstraintFinding, FindingDelta};
pub use resolver::ConstraintResolver;

use crate::db::Db;
use crate::error::Result;
use crate::rules::{Constraint, ConstraintKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub file_ids: Vec<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct DdFacts {
    pub import_edges: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintViolation {
    pub from_id: i64,
    pub to_id: i64,
    pub rule_from: String,
    pub rule_to: String,
}

#[derive(Debug, Clone, Default)]
pub struct DdDelta {
    pub added_edges: Vec<(i64, i64)>,
    pub removed_edges: Vec<(i64, i64)>,
}

pub fn find_matching_constraint<'a>(
    constraints: &'a [Constraint],
    from_path: &str,
    to_path: &str,
    file_to_component: &HashMap<&str, &str>,
    comp_name_to_id: &HashMap<&str, &str>,
) -> Option<&'a Constraint> {
    let opts = MatchOptions {
        require_literal_separator: true,
        ..MatchOptions::default()
    };
    constraints.iter().find(|c| match &c.kind {
        ConstraintKind::ForbiddenDep { from, to } => {
            Pattern::new(from)
                .ok()
                .is_some_and(|fp| fp.matches_with(from_path, opts))
                && Pattern::new(to)
                    .ok()
                    .is_some_and(|tp| tp.matches_with(to_path, opts))
        }
        ConstraintKind::Boundary {
            from_component,
            to_component,
        } => {
            let from_cid = file_to_component.get(from_path);
            let to_cid = file_to_component.get(to_path);
            let from_match = from_cid.is_some_and(|c| {
                c == from_component
                    || comp_name_to_id
                        .get(from_component.as_str())
                        .is_some_and(|id| id == c)
            });
            let to_match = to_cid.is_some_and(|c| {
                c == to_component
                    || comp_name_to_id
                        .get(to_component.as_str())
                        .is_some_and(|id| id == c)
            });
            from_match && to_match
        }
        _ => false,
    })
}

pub fn build_component_context(
    kind: &ConstraintKind,
    file_to_component: &HashMap<&str, &str>,
    from_path: &str,
    to_path: &str,
) -> Option<String> {
    match kind {
        ConstraintKind::Boundary {
            from_component,
            to_component,
        } => Some(format!("{from_component} -> {to_component}")),
        _ => {
            let from_c = file_to_component.get(from_path);
            let to_c = file_to_component.get(to_path);
            match (from_c, to_c) {
                (Some(f), Some(t)) if f != t => Some(format!("{f} -> {t}")),
                _ => None,
            }
        }
    }
}

/// Per-field match counts for a constraint's globs/components.
#[derive(Debug, Clone)]
pub struct ConstraintCoverage {
    pub fields: Vec<(&'static str, usize)>,
}

impl ConstraintCoverage {
    pub fn dead_fields(&self) -> Vec<&'static str> {
        self.fields
            .iter()
            .filter(|(_, count)| *count == 0)
            .map(|(name, _)| *name)
            .collect()
    }

    pub fn total_matched(&self) -> usize {
        self.fields.iter().map(|(_, c)| c).sum()
    }
}

pub fn constraint_coverage(
    constraint: &Constraint,
    paths: &[&str],
    component_names: &[&str],
    component_ids: &[&str],
) -> ConstraintCoverage {
    let glob_opts = MatchOptions {
        require_literal_separator: true,
        ..MatchOptions::default()
    };
    let count_glob = |pattern: &str| -> usize {
        Pattern::new(pattern).map_or(0, |pat| {
            paths
                .iter()
                .filter(|p| pat.matches_with(p, glob_opts))
                .count()
        })
    };
    let count_scope = |scope: &str| -> usize {
        paths
            .iter()
            .filter(|p| crate::rules::scope_matches_path(scope, p))
            .count()
    };

    let fields = match &constraint.kind {
        ConstraintKind::ForbiddenDep { from, to } => {
            vec![("from", count_glob(from)), ("to", count_glob(to))]
        }
        ConstraintKind::Boundary {
            from_component,
            to_component,
        } => {
            let from_exists = (component_names.contains(&from_component.as_str())
                || component_ids.contains(&from_component.as_str()))
                as usize;
            let to_exists = (component_names.contains(&to_component.as_str())
                || component_ids.contains(&to_component.as_str()))
                as usize;
            vec![("from_component", from_exists), ("to_component", to_exists)]
        }
        ConstraintKind::MaxFanIn { target, .. } => {
            vec![("target", count_glob(target))]
        }
        ConstraintKind::NoCycles => {
            if let Some(scope) = &constraint.scope {
                vec![("scope", count_scope(scope))]
            } else {
                vec![("scope", paths.len())]
            }
        }
        ConstraintKind::ForbiddenExternal { from, .. } => {
            vec![("from", count_glob(from))]
        }
        ConstraintKind::ConfinedExternal { allowed_in, .. } => {
            let total: usize = allowed_in.iter().map(|g| count_glob(g)).sum();
            vec![("allowed_in", total)]
        }
        ConstraintKind::ForbiddenPattern { language, .. } => {
            let registry = crate::parser::adapter::default_registry();
            let exts: Vec<&str> = registry
                .adapter_for_language(language)
                .map(|a| a.pattern_extensions().to_vec())
                .unwrap_or_default();
            let lang_paths: Vec<&&str> = paths
                .iter()
                .filter(|p| exts.iter().any(|ext| p.ends_with(&format!(".{ext}"))))
                .collect();
            let count = if let Some(scope) = &constraint.scope {
                lang_paths
                    .iter()
                    .filter(|p| crate::rules::scope_matches_path(scope, p))
                    .count()
            } else {
                lang_paths.len()
            };
            vec![("scope", count)]
        }
    };

    ConstraintCoverage { fields }
}

pub fn format_violation_detail(
    c: &Constraint,
    from: &str,
    to: &str,
    is_introduced: bool,
) -> String {
    let delta = if is_introduced { " [introduced]" } else { "" };
    match &c.kind {
        ConstraintKind::ForbiddenDep { from: rf, to: rt } => {
            format!("forbidden: {from} -> {to} (rule: {rf} -> {rt}){delta}")
        }
        ConstraintKind::Boundary {
            from_component,
            to_component,
        } => format!("boundary: {from} -> {to} ({from_component} -> {to_component}){delta}"),
        _ => format!("{}: {from} -> {to}{delta}", c.kind.kind_tag()),
    }
}

pub fn register_ratcheted_constraints(db: &Db, constraints: &[Constraint]) -> Result<usize> {
    let mut count = 0;
    for c in constraints {
        if !c.ratchet {
            continue;
        }
        db.upsert_constraint_ratchet(
            &c.id,
            c.name.as_deref(),
            &c.rendered_description(),
            c.severity.as_str(),
        )?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Severity;

    fn make_constraint(kind: ConstraintKind) -> Constraint {
        Constraint {
            id: "test1234".into(),
            kind,
            severity: Severity::Blocking,
            name: Some("test-rule".into()),
            provenance: None,
            scope: None,
            ratchet: false,
        }
    }

    #[test]
    fn coverage_forbidden_dep_both_match() {
        let c = make_constraint(ConstraintKind::ForbiddenDep {
            from: "src/tools/*".into(),
            to: "src/daemon.rs".into(),
        });
        let paths = vec![
            "src/tools/review.rs",
            "src/tools/orient.rs",
            "src/daemon.rs",
        ];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert_eq!(cov.fields, vec![("from", 2), ("to", 1)]);
        assert!(cov.dead_fields().is_empty());
    }

    #[test]
    fn coverage_forbidden_dep_dead_to() {
        let c = make_constraint(ConstraintKind::ForbiddenDep {
            from: "src/tools/*".into(),
            to: "src/old_daemon.rs".into(),
        });
        let paths = vec!["src/tools/review.rs", "src/daemon.rs"];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert_eq!(cov.dead_fields(), vec!["to"]);
    }

    #[test]
    fn coverage_forbidden_dep_dead_from() {
        let c = make_constraint(ConstraintKind::ForbiddenDep {
            from: "src/legacy/*".into(),
            to: "src/daemon.rs".into(),
        });
        let paths = vec!["src/tools/review.rs", "src/daemon.rs"];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert_eq!(cov.dead_fields(), vec!["from"]);
    }

    #[test]
    fn coverage_boundary_missing_component() {
        let c = make_constraint(ConstraintKind::Boundary {
            from_component: "db".into(),
            to_component: "http".into(),
        });
        let cov = constraint_coverage(&c, &[], &["db"], &[]);
        assert_eq!(cov.dead_fields(), vec!["to_component"]);
    }

    #[test]
    fn coverage_boundary_matched_by_id() {
        let c = make_constraint(ConstraintKind::Boundary {
            from_component: "comp-id-abc".into(),
            to_component: "comp-id-xyz".into(),
        });
        let cov = constraint_coverage(&c, &[], &[], &["comp-id-abc", "comp-id-xyz"]);
        assert!(cov.dead_fields().is_empty());
    }

    #[test]
    fn coverage_boundary_mixed_name_and_id() {
        let c = make_constraint(ConstraintKind::Boundary {
            from_component: "db".into(),
            to_component: "comp-id-xyz".into(),
        });
        let cov = constraint_coverage(&c, &[], &["db"], &["comp-id-xyz"]);
        assert!(cov.dead_fields().is_empty());
    }

    #[test]
    fn coverage_max_fan_in_dead_target() {
        let c = make_constraint(ConstraintKind::MaxFanIn {
            target: "src/deleted.rs".into(),
            threshold: 10,
        });
        let paths = vec!["src/lib.rs", "src/main.rs"];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert_eq!(cov.dead_fields(), vec!["target"]);
    }

    #[test]
    fn coverage_no_cycles_with_scope_zero_files() {
        let mut c = make_constraint(ConstraintKind::NoCycles);
        c.scope = Some("src/deleted/".into());
        let paths = vec!["src/lib.rs", "src/main.rs"];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert_eq!(cov.dead_fields(), vec!["scope"]);
    }

    #[test]
    fn coverage_no_cycles_without_scope_never_dead() {
        let c = make_constraint(ConstraintKind::NoCycles);
        let paths = vec!["src/lib.rs"];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert!(cov.dead_fields().is_empty());
    }

    #[test]
    fn coverage_no_cycles_with_glob_scope_counts_matches() {
        let mut c = make_constraint(ConstraintKind::NoCycles);
        c.scope = Some("src/**".into());
        let paths = vec!["src/lib.rs", "src/core/graph.rs", "tests/it.rs"];
        let cov = constraint_coverage(&c, &paths, &[], &[]);
        assert!(cov.dead_fields().is_empty());
        assert_eq!(cov.total_matched(), 2);
    }
}
