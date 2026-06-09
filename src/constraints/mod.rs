mod engine;
mod resolver;
mod worker;

use std::collections::HashMap;

use glob::{MatchOptions, Pattern};

pub use engine::DdEngine;
pub use resolver::ConstraintResolver;

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
    file_to_component: &HashMap<String, String>,
    comp_name_to_id: &HashMap<String, String>,
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
    file_to_component: &HashMap<String, String>,
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
