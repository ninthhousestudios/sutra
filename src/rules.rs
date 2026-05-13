use std::path::Path;

use serde::Deserialize;

use crate::error::{Result, SutraError};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    pub conventions: ConventionsConfig,
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
}
