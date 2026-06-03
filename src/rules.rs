use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use tracing::warn;

use crate::error::{Result, SutraError};

// --- Public types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Advisory,
    Informational,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintKind {
    ForbiddenDep { from: String, to: String },
    Boundary { from_component: String, to_component: String },
    MaxFanIn { target: String, threshold: u32 },
    NoCycles,
}

impl ConstraintKind {
    fn default_severity(&self) -> Severity {
        match self {
            Self::ForbiddenDep { .. } => Severity::Blocking,
            Self::Boundary { .. } => Severity::Blocking,
            Self::NoCycles => Severity::Blocking,
            Self::MaxFanIn { .. } => Severity::Advisory,
        }
    }

    fn kind_tag(&self) -> &'static str {
        match self {
            Self::ForbiddenDep { .. } => "forbidden_dep",
            Self::Boundary { .. } => "boundary",
            Self::MaxFanIn { .. } => "max_fan_in",
            Self::NoCycles => "no_cycles",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub id: String,
    pub kind: ConstraintKind,
    pub severity: Severity,
    pub name: Option<String>,
    pub provenance: Option<String>,
    pub scope: Option<String>,
}

impl Constraint {
    /// Identity = blake3(kind_tag, kind-specific params, scope). Scope is part of
    /// identity because constraints scoped to different paths are semantically distinct.
    fn compute_id(kind: &ConstraintKind, scope: Option<&str>) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind.kind_tag().as_bytes());
        hasher.update(b"\x00");
        match kind {
            ConstraintKind::ForbiddenDep { from, to } => {
                hasher.update(from.as_bytes());
                hasher.update(b"\x00");
                hasher.update(to.as_bytes());
            }
            ConstraintKind::Boundary { from_component, to_component } => {
                hasher.update(from_component.as_bytes());
                hasher.update(b"\x00");
                hasher.update(to_component.as_bytes());
            }
            ConstraintKind::MaxFanIn { target, threshold } => {
                hasher.update(target.as_bytes());
                hasher.update(b"\x00");
                hasher.update(&threshold.to_le_bytes());
            }
            ConstraintKind::NoCycles => {}
        }
        if let Some(s) = scope {
            hasher.update(b"\x00scope\x00");
            hasher.update(s.as_bytes());
        }
        hasher.finalize().to_hex()[..8].to_string()
    }
}

// --- TOML deserialization (raw) ---

#[derive(Debug, Clone, Deserialize)]
struct RawConstraint {
    kind: String,
    severity: Option<Severity>,
    name: Option<String>,
    provenance: Option<String>,
    scope: Option<String>,
    // forbidden_dep
    from: Option<String>,
    to: Option<String>,
    // boundary
    from_component: Option<String>,
    to_component: Option<String>,
    // max_fan_in
    target: Option<String>,
    threshold: Option<u32>,
}

impl RawConstraint {
    fn into_constraint(self) -> Result<Constraint> {
        let kind = match self.kind.as_str() {
            "forbidden_dep" => {
                let from = self.from.ok_or_else(|| {
                    SutraError::Internal("constraint kind 'forbidden_dep' requires 'from' field".into())
                })?;
                let to = self.to.ok_or_else(|| {
                    SutraError::Internal("constraint kind 'forbidden_dep' requires 'to' field".into())
                })?;
                ConstraintKind::ForbiddenDep { from, to }
            }
            "boundary" => {
                let from_component = self.from_component.ok_or_else(|| {
                    SutraError::Internal("constraint kind 'boundary' requires 'from_component' field".into())
                })?;
                let to_component = self.to_component.ok_or_else(|| {
                    SutraError::Internal("constraint kind 'boundary' requires 'to_component' field".into())
                })?;
                ConstraintKind::Boundary { from_component, to_component }
            }
            "max_fan_in" => {
                let target = self.target.ok_or_else(|| {
                    SutraError::Internal("constraint kind 'max_fan_in' requires 'target' field".into())
                })?;
                let threshold = self.threshold.ok_or_else(|| {
                    SutraError::Internal("constraint kind 'max_fan_in' requires 'threshold' field".into())
                })?;
                ConstraintKind::MaxFanIn { target, threshold }
            }
            "no_cycles" => ConstraintKind::NoCycles,
            other => {
                return Err(SutraError::Internal(format!(
                    "unknown constraint kind '{other}'"
                )));
            }
        };
        let severity = self.severity.unwrap_or_else(|| kind.default_severity());
        let id = Constraint::compute_id(&kind, self.scope.as_deref());
        Ok(Constraint {
            id,
            kind,
            severity,
            name: self.name,
            provenance: self.provenance,
            scope: self.scope,
        })
    }
}

// --- Rules (top-level TOML structure) ---

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    constraint: Vec<RawConstraint>,
    #[serde(default)]
    pub conventions: ConventionsConfig,
}

impl Rules {
    pub fn all_constraints(&self) -> Result<Vec<Constraint>> {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut out: Vec<Constraint> = Vec::new();

        for fd in &self.constraints.forbidden_deps {
            let kind = ConstraintKind::ForbiddenDep {
                from: fd.from.clone(),
                to: fd.to.clone(),
            };
            let id = Constraint::compute_id(&kind, None);
            if let Some(&idx) = seen.get(&id) {
                if out[idx].severity != Severity::Blocking {
                    warn!(id = %id, "duplicate constraint with different severity, keeping first");
                }
                continue;
            }
            seen.insert(id.clone(), out.len());
            out.push(Constraint {
                id,
                kind,
                severity: Severity::Blocking,
                name: None,
                provenance: None,
                scope: None,
            });
        }

        for raw in self.constraint.clone() {
            let c = raw.into_constraint()?;
            if let Some(&idx) = seen.get(&c.id) {
                if out[idx].severity != c.severity {
                    warn!(id = %c.id, "duplicate constraint with different severity, keeping first");
                }
                continue;
            }
            seen.insert(c.id.clone(), out.len());
            out.push(c);
        }

        Ok(out)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConventionsConfig {
    #[serde(default)]
    pub suppress: Vec<String>,
    #[serde(default)]
    pub exempt: Vec<ConventionExemption>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConventionExemption {
    pub convention: String,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Constraints {
    #[serde(default)]
    pub forbidden_deps: Vec<ForbiddenDep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ForbiddenDep {
    pub from: String,
    pub to: String,
}

pub fn parse_rules(content: &str) -> Result<Rules> {
    toml::from_str(content)
        .map_err(|e| SutraError::Internal(format!("rules.toml parse error: {e}")))
}

pub fn load_rules(root: &Path) -> Result<Rules> {
    let path = root.join(".sutra/rules.toml");
    if !path.exists() {
        return Ok(Rules::default());
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| SutraError::Internal(format!("{e}")))?;
    parse_rules(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forbidden_deps() {
        let toml = r#"
[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
  { from = "src/**", to = "src/internal/*" },
]
"#;
        let rules = parse_rules(toml).unwrap();
        assert_eq!(rules.constraints.forbidden_deps.len(), 2);
        assert_eq!(rules.constraints.forbidden_deps[0].from, "src/tools/*");
        assert_eq!(rules.constraints.forbidden_deps[0].to, "src/daemon.rs");
        assert_eq!(rules.constraints.forbidden_deps[1].from, "src/**");
        assert_eq!(rules.constraints.forbidden_deps[1].to, "src/internal/*");
    }

    #[test]
    fn parse_empty_constraints() {
        let toml = "[constraints]\n";
        let rules = parse_rules(toml).unwrap();
        assert!(rules.constraints.forbidden_deps.is_empty());
    }

    #[test]
    fn parse_missing_constraints_section() {
        let rules = parse_rules("").unwrap();
        assert!(rules.constraints.forbidden_deps.is_empty());
    }

    #[test]
    fn missing_file_returns_defaults() {
        let rules = load_rules(Path::new("/nonexistent/path")).unwrap();
        assert!(rules.constraints.forbidden_deps.is_empty());
    }

    #[test]
    fn parse_conventions_section() {
        let toml = r#"
[conventions]
suppress = ["a1b4c2d1", "b2c3d4e5"]

[[conventions.exempt]]
convention = "e5f6g7h8"
symbols = ["InternalError", "DebugHelper"]
"#;
        let rules = parse_rules(toml).unwrap();
        assert_eq!(rules.conventions.suppress, vec!["a1b4c2d1", "b2c3d4e5"]);
        assert_eq!(rules.conventions.exempt.len(), 1);
        assert_eq!(rules.conventions.exempt[0].convention, "e5f6g7h8");
        assert_eq!(
            rules.conventions.exempt[0].symbols,
            vec!["InternalError", "DebugHelper"]
        );
    }

    #[test]
    fn parse_missing_conventions_section() {
        let rules = parse_rules("[constraints]\n").unwrap();
        assert!(rules.conventions.suppress.is_empty());
        assert!(rules.conventions.exempt.is_empty());
    }

    #[test]
    fn parse_both_sections() {
        let toml = r#"
[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
]

[conventions]
suppress = ["abc123"]
"#;
        let rules = parse_rules(toml).unwrap();
        assert_eq!(rules.constraints.forbidden_deps.len(), 1);
        assert_eq!(rules.conventions.suppress, vec!["abc123"]);
    }

    // --- New constraint system tests ---

    #[test]
    fn parse_new_format_forbidden_dep() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
name = "no-tool-daemon"
provenance = "docs/adr-001.md"
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ConstraintKind::ForbiddenDep {
            from: "src/tools/*".into(),
            to: "src/daemon.rs".into(),
        });
        assert_eq!(cs[0].severity, Severity::Blocking);
        assert_eq!(cs[0].name.as_deref(), Some("no-tool-daemon"));
        assert_eq!(cs[0].provenance.as_deref(), Some("docs/adr-001.md"));
        assert!(!cs[0].id.is_empty());
    }

    #[test]
    fn parse_new_format_boundary() {
        let toml = r#"
[[constraint]]
kind = "boundary"
from_component = "db"
to_component = "http"
severity = "advisory"
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ConstraintKind::Boundary {
            from_component: "db".into(),
            to_component: "http".into(),
        });
        assert_eq!(cs[0].severity, Severity::Advisory);
    }

    #[test]
    fn parse_new_format_max_fan_in() {
        let toml = r#"
[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ConstraintKind::MaxFanIn {
            target: "src/config.rs".into(),
            threshold: 10,
        });
        assert_eq!(cs[0].severity, Severity::Advisory);
    }

    #[test]
    fn parse_new_format_no_cycles() {
        let toml = r#"
[[constraint]]
kind = "no_cycles"
scope = "src/core/"
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ConstraintKind::NoCycles);
        assert_eq!(cs[0].scope.as_deref(), Some("src/core/"));
        assert_eq!(cs[0].severity, Severity::Blocking);
    }

    #[test]
    fn old_format_produces_constraints() {
        let toml = r#"
[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
  { from = "src/**", to = "src/internal/*" },
]
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].kind, ConstraintKind::ForbiddenDep {
            from: "src/tools/*".into(),
            to: "src/daemon.rs".into(),
        });
        assert_eq!(cs[0].severity, Severity::Blocking);
        assert!(cs[0].name.is_none());
        assert!(cs[0].provenance.is_none());
    }

    #[test]
    fn mixed_old_and_new_format() {
        let toml = r#"
[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
]

[[constraint]]
kind = "boundary"
from_component = "db"
to_component = "http"

[[constraint]]
kind = "no_cycles"
scope = "src/core/"
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_eq!(cs.len(), 3);
        assert!(matches!(cs[0].kind, ConstraintKind::ForbiddenDep { .. }));
        assert!(matches!(cs[1].kind, ConstraintKind::Boundary { .. }));
        assert!(matches!(cs[2].kind, ConstraintKind::NoCycles));
    }

    #[test]
    fn identity_deterministic_and_stable() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        let id1 = &cs[0].id;

        let rules2 = parse_rules(toml).unwrap();
        let cs2 = rules2.all_constraints().unwrap();
        assert_eq!(id1, &cs2[0].id);
    }

    #[test]
    fn identity_differs_for_different_params() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"

[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "c"
"#;
        let rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().unwrap();
        assert_ne!(cs[0].id, cs[1].id);
    }

    #[test]
    fn name_does_not_affect_identity() {
        let toml1 = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
name = "alpha"
"#;
        let toml2 = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
name = "beta"
"#;
        let cs1 = parse_rules(toml1).unwrap().all_constraints().unwrap();
        let cs2 = parse_rules(toml2).unwrap().all_constraints().unwrap();
        assert_eq!(cs1[0].id, cs2[0].id);
    }

    #[test]
    fn severity_defaults_per_kind() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"

[[constraint]]
kind = "boundary"
from_component = "x"
to_component = "y"

[[constraint]]
kind = "max_fan_in"
target = "f.rs"
threshold = 5

[[constraint]]
kind = "no_cycles"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().unwrap();
        assert_eq!(cs[0].severity, Severity::Blocking);   // forbidden_dep
        assert_eq!(cs[1].severity, Severity::Blocking);   // boundary
        assert_eq!(cs[2].severity, Severity::Advisory);    // max_fan_in
        assert_eq!(cs[3].severity, Severity::Blocking);    // no_cycles
    }

    #[test]
    fn severity_override() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
severity = "informational"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().unwrap();
        assert_eq!(cs[0].severity, Severity::Informational);
    }

    #[test]
    fn unknown_kind_produces_error() {
        let toml = r#"
[[constraint]]
kind = "banana"
"#;
        let rules = parse_rules(toml).unwrap();
        let err = rules.all_constraints().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown constraint kind 'banana'"), "got: {msg}");
    }

    #[test]
    fn missing_required_field_produces_error() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
"#;
        let rules = parse_rules(toml).unwrap();
        let err = rules.all_constraints().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires 'to' field"), "got: {msg}");
    }

    #[test]
    fn duplicate_constraints_deduplicated() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
name = "first"

[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
name = "second"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].name.as_deref(), Some("first"));
    }

    #[test]
    fn dedup_across_old_and_new_format() {
        let toml = r#"
[constraints]
forbidden_deps = [
  { from = "a", to = "b" },
]

[[constraint]]
kind = "forbidden_dep"
from = "a"
to = "b"
name = "duplicate"
severity = "advisory"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().unwrap();
        assert_eq!(cs.len(), 1);
        // old format wins (processed first)
        assert!(cs[0].name.is_none());
        assert_eq!(cs[0].severity, Severity::Blocking);
    }
}
