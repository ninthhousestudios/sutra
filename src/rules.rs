use std::path::Path;

use serde::Deserialize;

use crate::error::{Result, SutraError};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub constraints: Constraints,
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
    toml::from_str(content).map_err(|e| SutraError::Internal(format!("rules.toml parse error: {e}")))
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
}
