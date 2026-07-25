use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use tracing::warn;

use crate::error::{Result, SutraError};
use crate::parser::adapter::{LanguageRegistry, default_registry};

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
    ForbiddenDep {
        from: String,
        to: String,
    },
    Boundary {
        from_component: String,
        to_component: String,
    },
    MaxFanIn {
        target: String,
        threshold: u32,
    },
    NoCycles,
    /// Forbid external crates/packages within a path scope.
    ForbiddenExternal {
        from: String,
        crates: Vec<String>,
        include_dev: bool,
    },
    /// External crates/packages importable ONLY from the listed paths.
    ConfinedExternal {
        crates: Vec<String>,
        allowed_in: Vec<String>,
        include_dev: bool,
    },
    /// Forbid tree-sitter patterns from appearing in source files.
    ForbiddenPattern {
        language: String,
        query: String,
    },
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Advisory => "advisory",
            Self::Informational => "informational",
        }
    }

    pub fn ordinal(self) -> u8 {
        match self {
            Self::Informational => 0,
            Self::Advisory => 1,
            Self::Blocking => 2,
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "blocking" => Some(Self::Blocking),
            "advisory" => Some(Self::Advisory),
            "informational" => Some(Self::Informational),
            _ => None,
        }
    }
}

impl ConstraintKind {
    fn default_severity(&self) -> Severity {
        match self {
            Self::ForbiddenDep { .. } => Severity::Blocking,
            Self::Boundary { .. } => Severity::Blocking,
            Self::NoCycles => Severity::Blocking,
            Self::MaxFanIn { .. } => Severity::Advisory,
            Self::ForbiddenExternal { .. } => Severity::Blocking,
            Self::ConfinedExternal { .. } => Severity::Blocking,
            Self::ForbiddenPattern { .. } => Severity::Advisory,
        }
    }

    pub fn kind_tag(&self) -> &'static str {
        match self {
            Self::ForbiddenDep { .. } => "forbidden_dep",
            Self::Boundary { .. } => "boundary",
            Self::MaxFanIn { .. } => "max_fan_in",
            Self::NoCycles => "no_cycles",
            Self::ForbiddenExternal { .. } => "forbidden_external",
            Self::ConfinedExternal { .. } => "confined_external",
            Self::ForbiddenPattern { .. } => "forbidden_pattern",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub id: Arc<str>,
    pub kind: ConstraintKind,
    pub severity: Severity,
    pub name: Option<Arc<str>>,
    pub provenance: Option<Arc<str>>,
    pub scope: Option<String>,
    pub ratchet: bool,
    /// Opt in to evaluating test-only code (Rust `#[cfg(test)]` items and the
    /// imports inside them). Off by default: test code exercises the very
    /// patterns production rules forbid, so including it drowns real signal.
    /// Excluded from constraint identity — toggling it must not orphan waivers
    /// or ratchet registrations.
    pub include_tests: bool,
}

impl Constraint {
    /// Identity = blake3(kind_tag, kind-specific params, scope). Scope is part of
    /// identity because constraints scoped to different paths are semantically distinct.
    fn compute_id(kind: &ConstraintKind, scope: Option<&str>) -> Arc<str> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind.kind_tag().as_bytes());
        hasher.update(b"\x00");
        match kind {
            ConstraintKind::ForbiddenDep { from, to } => {
                hasher.update(from.as_bytes());
                hasher.update(b"\x00");
                hasher.update(to.as_bytes());
            }
            ConstraintKind::Boundary {
                from_component,
                to_component,
            } => {
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
            ConstraintKind::ForbiddenExternal {
                from,
                crates,
                include_dev,
            } => {
                hasher.update(from.as_bytes());
                for c in crates {
                    hasher.update(b"\x00");
                    hasher.update(c.as_bytes());
                }
                hasher.update(b"\x00");
                hasher.update(&[*include_dev as u8]);
            }
            ConstraintKind::ConfinedExternal {
                crates,
                allowed_in,
                include_dev,
            } => {
                for c in crates {
                    hasher.update(c.as_bytes());
                    hasher.update(b"\x00");
                }
                hasher.update(b"\x00allowed\x00");
                for a in allowed_in {
                    hasher.update(a.as_bytes());
                    hasher.update(b"\x00");
                }
                hasher.update(&[*include_dev as u8]);
            }
            ConstraintKind::ForbiddenPattern { language, query } => {
                hasher.update(language.as_bytes());
                hasher.update(b"\x00");
                hasher.update(query.as_bytes());
            }
        }
        if let Some(s) = scope {
            hasher.update(b"\x00scope\x00");
            hasher.update(s.as_bytes());
        }
        Arc::from(&hasher.finalize().to_hex()[..8])
    }

    pub fn rendered_description(&self) -> String {
        let kind_desc = match &self.kind {
            ConstraintKind::ForbiddenDep { from, to } => {
                format!("forbidden_dep: {from} → {to}")
            }
            ConstraintKind::Boundary {
                from_component,
                to_component,
            } => format!("boundary: {from_component} → {to_component}"),
            ConstraintKind::MaxFanIn { target, threshold } => {
                format!("max_fan_in: {target} ≤ {threshold}")
            }
            ConstraintKind::NoCycles => "no_cycles".to_string(),
            ConstraintKind::ForbiddenExternal { from, crates, .. } => {
                format!("forbidden_external: {} from {from}", crates.join(", "))
            }
            ConstraintKind::ConfinedExternal {
                crates, allowed_in, ..
            } => {
                format!(
                    "confined_external: {} only in {}",
                    crates.join(", "),
                    allowed_in.join(", ")
                )
            }
            ConstraintKind::ForbiddenPattern { language, query } => {
                if query.chars().count() > 60 {
                    let truncated: String = query.chars().take(60).collect();
                    format!("forbidden_pattern({language}): {truncated}…")
                } else {
                    format!("forbidden_pattern({language}): {query}")
                }
            }
        };
        match &self.scope {
            Some(s) => format!("{kind_desc} [scope: {s}]"),
            None => kind_desc,
        }
    }
}

fn validate_glob(field: &str, pattern: &str) -> Result<()> {
    glob::Pattern::new(pattern).map_err(|e| {
        SutraError::Internal(format!("invalid glob in '{field}': '{pattern}': {e}"))
    })?;
    Ok(())
}

fn validate_crate_glob(field: &str, pattern: &str) -> Result<()> {
    let normalized = pattern.replace('-', "_");
    glob::Pattern::new(&normalized).map_err(|e| {
        SutraError::Internal(format!(
            "invalid crate glob in '{field}': '{pattern}' (normalized: '{normalized}'): {e}"
        ))
    })?;
    Ok(())
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
    // forbidden_external / confined_external
    crates: Option<Vec<String>>,
    allowed_in: Option<Vec<String>>,
    include_dev: Option<bool>,
    // forbidden_pattern
    language: Option<String>,
    query: Option<String>,
    // ratchet
    ratchet: Option<bool>,
    // test-code opt-in
    include_tests: Option<bool>,
}

impl RawConstraint {
    fn into_constraint(self, registry: &LanguageRegistry) -> Result<Constraint> {
        let kind = match self.kind.as_str() {
            "forbidden_dep" => {
                let from = self.from.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'forbidden_dep' requires 'from' field".into(),
                    )
                })?;
                let to = self.to.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'forbidden_dep' requires 'to' field".into(),
                    )
                })?;
                validate_glob("from", &from)?;
                validate_glob("to", &to)?;
                ConstraintKind::ForbiddenDep { from, to }
            }
            "boundary" => {
                let from_component = self.from_component.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'boundary' requires 'from_component' field".into(),
                    )
                })?;
                let to_component = self.to_component.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'boundary' requires 'to_component' field".into(),
                    )
                })?;
                ConstraintKind::Boundary {
                    from_component,
                    to_component,
                }
            }
            "max_fan_in" => {
                let target = self.target.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'max_fan_in' requires 'target' field".into(),
                    )
                })?;
                let threshold = self.threshold.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'max_fan_in' requires 'threshold' field".into(),
                    )
                })?;
                validate_glob("target", &target)?;
                ConstraintKind::MaxFanIn { target, threshold }
            }
            "no_cycles" => ConstraintKind::NoCycles,
            "forbidden_external" => {
                let crates = match self.crates {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Err(SutraError::Internal(
                            "constraint kind 'forbidden_external' requires a non-empty 'crates' list"
                                .into(),
                        ));
                    }
                };
                let from = self.from.unwrap_or_else(|| "**".into());
                validate_glob("from", &from)?;
                for (i, c) in crates.iter().enumerate() {
                    validate_crate_glob(&format!("crates[{i}]"), c)?;
                }
                ConstraintKind::ForbiddenExternal {
                    from,
                    crates,
                    include_dev: self.include_dev.unwrap_or(false),
                }
            }
            "confined_external" => {
                let crates = match self.crates {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Err(SutraError::Internal(
                            "constraint kind 'confined_external' requires a non-empty 'crates' list"
                                .into(),
                        ));
                    }
                };
                let allowed_in = self.allowed_in.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'confined_external' requires 'allowed_in' field \
                         (empty list = banned everywhere)"
                            .into(),
                    )
                })?;
                for (i, c) in crates.iter().enumerate() {
                    validate_crate_glob(&format!("crates[{i}]"), c)?;
                }
                for (i, a) in allowed_in.iter().enumerate() {
                    validate_glob(&format!("allowed_in[{i}]"), a)?;
                }
                ConstraintKind::ConfinedExternal {
                    crates,
                    allowed_in,
                    include_dev: self.include_dev.unwrap_or(false),
                }
            }
            "forbidden_pattern" => {
                let language = self.language.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'forbidden_pattern' requires 'language' field".into(),
                    )
                })?;
                let query = self.query.ok_or_else(|| {
                    SutraError::Internal(
                        "constraint kind 'forbidden_pattern' requires 'query' field".into(),
                    )
                })?;
                let adapter = registry.adapter_for_language(&language).ok_or_else(|| {
                    SutraError::Internal(format!(
                        "constraint kind 'forbidden_pattern': unknown language '{language}'"
                    ))
                })?;
                tree_sitter::Query::new(&adapter.grammar(), &query).map_err(|e| {
                    SutraError::Internal(format!(
                        "constraint kind 'forbidden_pattern': invalid query for language \
                         '{language}': {e}"
                    ))
                })?;
                ConstraintKind::ForbiddenPattern { language, query }
            }
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
            name: self.name.map(Arc::from),
            provenance: self.provenance.map(Arc::from),
            scope: self.scope,
            ratchet: self.ratchet.unwrap_or(false),
            include_tests: self.include_tests.unwrap_or(false),
        })
    }
}

// --- Rules (top-level TOML structure) ---

#[derive(Debug, Clone)]
pub struct ConstraintParseError {
    pub index: usize,
    pub name: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PythonConfig {
    pub package_roots: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RatchetConfig {
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    constraint: Vec<RawConstraint>,
    #[serde(default)]
    pub conventions: ConventionsConfig,
    #[serde(default)]
    pub python: Option<PythonConfig>,
    #[serde(default)]
    pub ratchet: RatchetConfig,
}

impl Rules {
    pub fn all_constraints(&mut self) -> (Vec<Constraint>, Vec<ConstraintParseError>) {
        let registry = default_registry();
        let mut seen: HashMap<Arc<str>, usize> = HashMap::new();
        let mut out: Vec<Constraint> = Vec::new();
        let mut errors: Vec<ConstraintParseError> = Vec::new();

        for (i, fd) in self.constraints.forbidden_deps.iter().enumerate() {
            if let Err(e) =
                validate_glob("from", &fd.from).and_then(|()| validate_glob("to", &fd.to))
            {
                errors.push(ConstraintParseError {
                    index: i,
                    name: None,
                    error: format!("forbidden_deps[{i}]: {e}"),
                });
                continue;
            }
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
                ratchet: self.ratchet.all,
                include_tests: false,
            });
        }

        let ratchet_all = self.ratchet.all;
        for (i, raw) in std::mem::take(&mut self.constraint).into_iter().enumerate() {
            let name = raw.name.clone();
            match raw.into_constraint(&registry) {
                Ok(mut c) => {
                    c.ratchet = c.ratchet || ratchet_all;
                    if let Some(&idx) = seen.get(&c.id) {
                        if out[idx].severity != c.severity {
                            warn!(id = %c.id, "duplicate constraint with different severity, keeping first");
                        }
                        continue;
                    }
                    seen.insert(c.id.clone(), out.len());
                    out.push(c);
                }
                Err(e) => {
                    errors.push(ConstraintParseError {
                        index: i,
                        name,
                        error: e.to_string(),
                    });
                }
            }
        }

        (out, errors)
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

/// Match a path against a constraint `scope`, which is either a directory
/// prefix or a glob. The literal interpretation is tried first so real
/// directories containing glob metacharacters (e.g. `src/app/[slug]/`) keep
/// matching; glob matching is the fallback:
///
/// - Directory-prefix match with a trailing-slash boundary: `src/core`
///   matches `src/core/x.rs` and the exact path `src/core`, but not
///   `src/corely.rs`.
/// - If that fails and the scope contains glob metacharacters (`*`, `?`,
///   `[`) → glob match with `require_literal_separator` (same options as
///   every other glob field, so `src/*` stays within one directory level
///   and `src/**` recurses).
pub fn scope_matches_path(scope: &str, path: &str) -> bool {
    if path == scope {
        return true;
    }
    let stripped = scope.strip_suffix('/').unwrap_or(scope);
    if path
        .strip_prefix(stripped)
        .is_some_and(|rest| rest.starts_with('/'))
    {
        return true;
    }
    if scope.contains(['*', '?', '[']) {
        let opts = glob::MatchOptions {
            require_literal_separator: true,
            ..glob::MatchOptions::default()
        };
        return glob::Pattern::new(scope).is_ok_and(|pat| pat.matches_with(path, opts));
    }
    false
}

/// Match a cycle (given as resolved paths) to the best-fitting `NoCycles` constraint.
///
/// - Unscoped (`scope: None`) matches all cycles.
/// - Scoped matches only if every path in the cycle is within the scope
///   (glob or directory prefix — see [`scope_matches_path`]).
/// - When multiple constraints match, the longest (most specific) scope wins.
/// - Returns `None` when no `NoCycles` constraint covers this cycle.
pub fn match_no_cycles_constraint<'a>(
    constraints: &'a [Constraint],
    cycle_paths: &[&str],
) -> Option<&'a Constraint> {
    constraints
        .iter()
        .filter(|c| matches!(c.kind, ConstraintKind::NoCycles))
        .filter(|c| match &c.scope {
            None => true,
            Some(scope) => cycle_paths.iter().all(|p| scope_matches_path(scope, p)),
        })
        .max_by_key(|c| c.scope.as_ref().map_or(0, |s| s.len()))
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs[0].kind,
            ConstraintKind::ForbiddenDep {
                from: "src/tools/*".into(),
                to: "src/daemon.rs".into(),
            }
        );
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs[0].kind,
            ConstraintKind::Boundary {
                from_component: "db".into(),
                to_component: "http".into(),
            }
        );
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs[0].kind,
            ConstraintKind::MaxFanIn {
                target: "src/config.rs".into(),
                threshold: 10,
            }
        );
        assert_eq!(cs[0].severity, Severity::Advisory);
    }

    #[test]
    fn parse_new_format_no_cycles() {
        let toml = r#"
[[constraint]]
kind = "no_cycles"
scope = "src/core/"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
        assert_eq!(cs.len(), 2);
        assert_eq!(
            cs[0].kind,
            ConstraintKind::ForbiddenDep {
                from: "src/tools/*".into(),
                to: "src/daemon.rs".into(),
            }
        );
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
        let id1 = &cs[0].id;

        let mut rules2 = parse_rules(toml).unwrap();
        let cs2 = rules2.all_constraints().0;
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
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
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
        let cs1 = parse_rules(toml1).unwrap().all_constraints().0;
        let cs2 = parse_rules(toml2).unwrap().all_constraints().0;
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
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs[0].severity, Severity::Blocking); // forbidden_dep
        assert_eq!(cs[1].severity, Severity::Blocking); // boundary
        assert_eq!(cs[2].severity, Severity::Advisory); // max_fan_in
        assert_eq!(cs[3].severity, Severity::Blocking); // no_cycles
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
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs[0].severity, Severity::Informational);
    }

    #[test]
    fn unknown_kind_produces_error() {
        let toml = r#"
[[constraint]]
kind = "banana"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let (valid, errors) = rules.all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].error.contains("unknown constraint kind 'banana'"),
            "got: {}",
            errors[0].error
        );
    }

    #[test]
    fn missing_required_field_produces_error() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "a"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let (valid, errors) = rules.all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].error.contains("requires 'to' field"),
            "got: {}",
            errors[0].error
        );
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
        let cs = parse_rules(toml).unwrap().all_constraints().0;
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
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs.len(), 1);
        // old format wins (processed first)
        assert!(cs[0].name.is_none());
        assert_eq!(cs[0].severity, Severity::Blocking);
    }

    #[test]
    fn parse_forbidden_external_with_defaults() {
        let toml = r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow_core", "arrow_swe"]
name = "agpl-boundary"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs[0].kind,
            ConstraintKind::ForbiddenExternal {
                from: "**".into(),
                crates: vec!["arrow_core".into(), "arrow_swe".into()],
                include_dev: false,
            }
        );
        assert_eq!(cs[0].severity, Severity::Blocking);
    }

    #[test]
    fn parse_forbidden_external_scoped() {
        let toml = r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum"]
include_dev = true
severity = "advisory"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(
            cs[0].kind,
            ConstraintKind::ForbiddenExternal {
                from: "report/**".into(),
                crates: vec!["axum".into()],
                include_dev: true,
            }
        );
        assert_eq!(cs[0].severity, Severity::Advisory);
    }

    #[test]
    fn parse_confined_external() {
        let toml = r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic", "prost"]
allowed_in = ["quiver-client/**"]
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(
            cs[0].kind,
            ConstraintKind::ConfinedExternal {
                crates: vec!["tonic".into(), "prost".into()],
                allowed_in: vec!["quiver-client/**".into()],
                include_dev: false,
            }
        );
        assert_eq!(cs[0].severity, Severity::Blocking);
    }

    #[test]
    fn forbidden_external_requires_crates() {
        let toml = r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert!(errors[0].error.contains("non-empty 'crates'"));
    }

    #[test]
    fn confined_external_requires_allowed_in() {
        let toml = r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic"]
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert!(errors[0].error.contains("allowed_in"));
    }

    #[test]
    fn confined_external_empty_allowed_in_is_valid() {
        let toml = r#"
[[constraint]]
kind = "confined_external"
crates = ["leftpad"]
allowed_in = []
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert_eq!(valid.len(), 1);
        assert!(errors.is_empty());
    }

    // --- glob validation tests ---

    #[test]
    fn invalid_glob_in_forbidden_dep_from() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/[bad"
to = "src/ok.rs"
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("invalid glob"));
        assert!(errors[0].error.contains("from"));
    }

    #[test]
    fn invalid_glob_in_forbidden_dep_to() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/**"
to = "lib/[unclosed"
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("invalid glob"));
        assert!(errors[0].error.contains("to"));
    }

    #[test]
    fn invalid_glob_in_max_fan_in_target() {
        let toml = r#"
[[constraint]]
kind = "max_fan_in"
target = "src/[bad"
threshold = 5
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("invalid glob"));
        assert!(errors[0].error.contains("target"));
    }

    #[test]
    fn invalid_glob_in_forbidden_external_crates() {
        let toml = r#"
[[constraint]]
kind = "forbidden_external"
crates = ["good*", "bad["]
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("invalid crate glob"));
        assert!(errors[0].error.contains("crates[1]"));
    }

    #[test]
    fn invalid_glob_in_forbidden_external_from() {
        let toml = r#"
[[constraint]]
kind = "forbidden_external"
crates = ["leftpad"]
from = "report/[unclosed"
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("invalid glob"));
        assert!(errors[0].error.contains("from"));
    }

    #[test]
    fn invalid_glob_in_confined_external_allowed_in() {
        let toml = r#"
[[constraint]]
kind = "confined_external"
crates = ["stripe"]
allowed_in = ["src/payments/**", "lib/[bad"]
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("invalid glob"));
        assert!(errors[0].error.contains("allowed_in[1]"));
    }

    #[test]
    fn invalid_glob_in_old_format_forbidden_deps() {
        let toml = r#"
[constraints]
forbidden_deps = [
  { from = "src/[bad", to = "src/ok.rs" },
]
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("forbidden_deps[0]"));
    }

    #[test]
    fn valid_globs_pass_validation() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/**/*.rs"
to = "src/daemon.rs"

[[constraint]]
kind = "max_fan_in"
target = "src/config.*"
threshold = 10

[[constraint]]
kind = "forbidden_external"
crates = ["arrow-*", "tokio"]
from = "report/**"

[[constraint]]
kind = "confined_external"
crates = ["stripe*"]
allowed_in = ["src/payments/**", "src/billing/**"]
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert_eq!(valid.len(), 4);
        assert!(errors.is_empty());
    }

    #[test]
    fn crate_glob_with_hyphens_validates_after_normalization() {
        let toml = r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-flight-*"]
"#;
        let (valid, errors) = parse_rules(toml).unwrap().all_constraints();
        assert_eq!(valid.len(), 1);
        assert!(errors.is_empty());
    }

    #[test]
    fn scope_glob_recursive_matches_nested_and_flat() {
        assert!(scope_matches_path("src/**", "src/lib.rs"));
        assert!(scope_matches_path("src/**", "src/core/deep/mod.rs"));
        assert!(!scope_matches_path("src/**", "tests/lib.rs"));
    }

    #[test]
    fn scope_glob_single_star_stays_flat() {
        assert!(scope_matches_path("src/*.rs", "src/lib.rs"));
        assert!(!scope_matches_path("src/*.rs", "src/core/mod.rs"));
        assert!(scope_matches_path("src/**/*.rs", "src/core/mod.rs"));
    }

    #[test]
    fn scope_prefix_matches_directory_contents() {
        assert!(scope_matches_path("src/core/", "src/core/graph.rs"));
        assert!(scope_matches_path("src/core", "src/core/graph.rs"));
        assert!(scope_matches_path("src/core", "src/core"));
    }

    #[test]
    fn scope_prefix_respects_path_boundary() {
        assert!(!scope_matches_path("src/core", "src/corely.rs"));
        assert!(!scope_matches_path("src/core/", "src/corely.rs"));
    }

    #[test]
    fn scope_invalid_glob_matches_nothing() {
        assert!(!scope_matches_path("src/[", "src/lib.rs"));
    }

    #[test]
    fn scope_literal_bracket_directory_matches_literally() {
        assert!(scope_matches_path(
            "src/app/[slug]/",
            "src/app/[slug]/page.tsx"
        ));
        assert!(scope_matches_path("src/app/[slug]", "src/app/[slug]"));
        assert!(!scope_matches_path(
            "src/app/[slug]/",
            "src/app/other/page.tsx"
        ));
    }

    #[test]
    fn scope_literal_match_wins_before_glob_interpretation() {
        // An invalid glob that names a real path still matches it literally.
        assert!(scope_matches_path("src/[", "src/[/file.rs"));
        // Glob fallback still applies when the literal match fails.
        assert!(scope_matches_path("src/app/[slug]", "src/app/s"));
    }

    fn no_cycles_constraints(toml: &str) -> Vec<Constraint> {
        parse_rules(toml).unwrap().all_constraints().0
    }

    #[test]
    fn no_cycles_glob_scope_binds_cycle() {
        let cs = no_cycles_constraints(
            r#"
[[constraint]]
kind = "no_cycles"
scope = "src/**"
"#,
        );
        let m = match_no_cycles_constraint(&cs, &["src/a.rs", "src/b.rs"]);
        assert_eq!(m.map(|c| c.id.as_ref()), Some(cs[0].id.as_ref()));
    }

    #[test]
    fn no_cycles_glob_scope_rejects_cycle_with_outside_path() {
        let cs = no_cycles_constraints(
            r#"
[[constraint]]
kind = "no_cycles"
scope = "src/**"
"#,
        );
        assert!(match_no_cycles_constraint(&cs, &["src/a.rs", "tests/b.rs"]).is_none());
    }

    #[test]
    fn no_cycles_longest_scope_wins_across_glob_and_prefix() {
        let cs = no_cycles_constraints(
            r#"
[[constraint]]
kind = "no_cycles"
scope = "src/**"

[[constraint]]
kind = "no_cycles"
scope = "src/core/"
"#,
        );
        let m = match_no_cycles_constraint(&cs, &["src/core/a.rs", "src/core/b.rs"]).unwrap();
        assert_eq!(m.scope.as_deref(), Some("src/core/"));
    }

    #[test]
    fn no_cycles_unscoped_matches_any_cycle() {
        let cs = no_cycles_constraints(
            r#"
[[constraint]]
kind = "no_cycles"
"#,
        );
        assert!(match_no_cycles_constraint(&cs, &["src/a.rs", "vendor/b.rs"]).is_some());
    }

    #[test]
    fn parse_forbidden_pattern() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
name = "no-unsafe"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let cs = rules.all_constraints().0;
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs[0].kind,
            ConstraintKind::ForbiddenPattern {
                language: "rust".into(),
                query: "(unsafe_block) @cap".into(),
            }
        );
        assert_eq!(cs[0].severity, Severity::Advisory);
        assert_eq!(cs[0].name.as_deref(), Some("no-unsafe"));
    }

    #[test]
    fn forbidden_pattern_severity_default_is_advisory() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs[0].severity, Severity::Advisory);
    }

    #[test]
    fn forbidden_pattern_severity_override() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
severity = "blocking"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs[0].severity, Severity::Blocking);
    }

    #[test]
    fn forbidden_pattern_requires_language() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
query = "(unsafe_block) @cap"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let (valid, errors) = rules.all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].error.contains("requires 'language' field"),
            "got: {}",
            errors[0].error
        );
    }

    #[test]
    fn forbidden_pattern_requires_query() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let (valid, errors) = rules.all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].error.contains("requires 'query' field"),
            "got: {}",
            errors[0].error
        );
    }

    #[test]
    fn forbidden_pattern_unknown_language_produces_error() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "brainfuck"
query = "(something) @cap"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let (valid, errors) = rules.all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].error.contains("unknown language 'brainfuck'"),
            "got: {}",
            errors[0].error
        );
    }

    #[test]
    fn forbidden_pattern_invalid_query_produces_error() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(not_a_real_node_type) @cap"
"#;
        let mut rules = parse_rules(toml).unwrap();
        let (valid, errors) = rules.all_constraints();
        assert!(valid.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].error.contains("invalid query"),
            "got: {}",
            errors[0].error
        );
    }

    #[test]
    fn forbidden_pattern_identity_deterministic() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let id1 = parse_rules(toml).unwrap().all_constraints().0[0].id.clone();
        let id2 = parse_rules(toml).unwrap().all_constraints().0[0].id.clone();
        assert_eq!(id1, id2);
    }

    #[test]
    fn forbidden_pattern_identity_differs_for_different_query() {
        let toml1 = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let toml2 = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(function_item) @cap"
"#;
        let id1 = parse_rules(toml1).unwrap().all_constraints().0[0]
            .id
            .clone();
        let id2 = parse_rules(toml2).unwrap().all_constraints().0[0]
            .id
            .clone();
        assert_ne!(id1, id2);
    }

    #[test]
    fn forbidden_pattern_identity_differs_for_different_language() {
        let toml1 = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(function_item) @cap"
"#;
        let toml2 = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = "(function_definition) @cap"
"#;
        let id1 = parse_rules(toml1).unwrap().all_constraints().0[0]
            .id
            .clone();
        let id2 = parse_rules(toml2).unwrap().all_constraints().0[0]
            .id
            .clone();
        assert_ne!(id1, id2);
    }

    #[test]
    fn forbidden_pattern_with_scope() {
        let toml = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
scope = "src/core"
"#;
        let cs = parse_rules(toml).unwrap().all_constraints().0;
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].scope.as_deref(), Some("src/core"));
    }

    #[test]
    fn forbidden_pattern_scope_affects_identity() {
        let toml1 = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
"#;
        let toml2 = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = "(unsafe_block) @cap"
scope = "src/core"
"#;
        let id1 = parse_rules(toml1).unwrap().all_constraints().0[0]
            .id
            .clone();
        let id2 = parse_rules(toml2).unwrap().all_constraints().0[0]
            .id
            .clone();
        assert_ne!(id1, id2);
    }

    // --- Ratchet flag parsing ---

    #[test]
    fn per_constraint_ratchet_flag() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/a"
to = "src/b"
ratchet = true
"#;
        let (constraints, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(errors.is_empty());
        assert_eq!(constraints.len(), 1);
        assert!(constraints[0].ratchet);
    }

    #[test]
    fn per_constraint_ratchet_defaults_false() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/a"
to = "src/b"
"#;
        let (constraints, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(errors.is_empty());
        assert!(!constraints[0].ratchet);
    }

    #[test]
    fn workspace_ratchet_all() {
        let toml = r#"
[ratchet]
all = true

[[constraint]]
kind = "forbidden_dep"
from = "src/a"
to = "src/b"

[[constraint]]
kind = "no_cycles"
"#;
        let (constraints, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(errors.is_empty());
        assert_eq!(constraints.len(), 2);
        assert!(constraints[0].ratchet);
        assert!(constraints[1].ratchet);
    }

    #[test]
    fn workspace_ratchet_all_applies_to_old_format() {
        let toml = r#"
[ratchet]
all = true

[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
]
"#;
        let (constraints, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(errors.is_empty());
        assert!(constraints[0].ratchet);
    }

    #[test]
    fn per_constraint_ratchet_without_workspace_all() {
        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/a"
to = "src/b"
ratchet = true

[[constraint]]
kind = "no_cycles"
"#;
        let (constraints, errors) = parse_rules(toml).unwrap().all_constraints();
        assert!(errors.is_empty());
        assert!(constraints[0].ratchet);
        assert!(!constraints[1].ratchet);
    }
}
