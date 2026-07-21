use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use tracing::debug;

#[derive(Debug, Default)]
pub struct TsConfig {
    pub base_url: Option<String>,
    pub paths: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RawTsConfig {
    extends: Option<String>,
    #[serde(rename = "compilerOptions")]
    compiler_options: Option<RawCompilerOptions>,
}

#[derive(Deserialize, Default)]
struct RawCompilerOptions {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    paths: Option<HashMap<String, Vec<String>>>,
}

impl TsConfig {
    pub fn load(workspace_root: &Path) -> Option<Self> {
        for name in &["tsconfig.json", "jsconfig.json"] {
            let path = workspace_root.join(name);
            if path.exists() {
                match Self::parse_chain(&path, 0) {
                    Some(cfg) => return Some(cfg),
                    None => continue,
                }
            }
        }
        None
    }

    fn parse_chain(path: &Path, depth: u8) -> Option<Self> {
        if depth > 10 {
            debug!(?path, "tsconfig extends chain too deep, stopping");
            return None;
        }

        let content = std::fs::read_to_string(path).ok()?;
        let raw: RawTsConfig = serde_json::from_str(&content).ok()?;

        let config_dir = path.parent()?;

        let mut base = if let Some(extends) = &raw.extends {
            if extends.starts_with('.') {
                let parent_path = config_dir.join(extends);
                let parent_path = if parent_path.extension().is_none() {
                    parent_path.with_extension("json")
                } else {
                    parent_path
                };
                Self::parse_chain(&parent_path, depth + 1).unwrap_or_default()
            } else {
                Self::default()
            }
        } else {
            Self::default()
        };

        if let Some(opts) = raw.compiler_options {
            if let Some(base_url_str) = opts.base_url {
                let normalized = if base_url_str == "." {
                    String::new()
                } else {
                    base_url_str
                };
                base.base_url = Some(normalized);
            }
            if let Some(paths) = opts.paths {
                base.paths = paths;
            }
        }

        Some(base)
    }

    pub fn resolve_specifier(
        &self,
        specifier: &str,
        path_to_id: &HashMap<&str, i64>,
        try_resolve: &dyn Fn(&str, &HashMap<&str, i64>) -> Option<i64>,
    ) -> Option<i64> {
        for (pattern, targets) in &self.paths {
            if let Some(captured) = match_pattern(pattern, specifier) {
                for target in targets {
                    let substituted = target.replace('*', captured);
                    let candidate = self.with_base_url(&substituted);
                    if let Some(id) = try_resolve(&candidate, path_to_id) {
                        return Some(id);
                    }
                }
            }
        }

        if let Some(ref _base_url) = self.base_url {
            let candidate = self.with_base_url(specifier);
            if let Some(id) = try_resolve(&candidate, path_to_id) {
                return Some(id);
            }
        }

        None
    }

    fn with_base_url(&self, relative: &str) -> String {
        match &self.base_url {
            Some(base) if !base.is_empty() => format!("{base}/{relative}"),
            _ => relative.to_string(),
        }
    }
}

fn match_pattern<'a>(pattern: &str, specifier: &'a str) -> Option<&'a str> {
    if let Some(star_pos) = pattern.find('*') {
        let prefix = &pattern[..star_pos];
        let suffix = &pattern[star_pos + 1..];
        if specifier.starts_with(prefix) && specifier.ends_with(suffix) {
            let end = specifier.len() - suffix.len();
            if end >= prefix.len() {
                return Some(&specifier[prefix.len()..end]);
            }
        }
    } else if pattern == specifier {
        return Some("");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_pattern() {
        assert_eq!(
            match_pattern("@/*", "@/components/Foo"),
            Some("components/Foo")
        );
        assert_eq!(match_pattern("@utils/*", "@utils/helper"), Some("helper"));
        assert_eq!(match_pattern("exact-match", "exact-match"), Some(""));
        assert_eq!(match_pattern("@/*", "react"), None);
        assert_eq!(match_pattern("@/*", "@"), None);
    }

    #[test]
    fn test_load_tsconfig() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": {
                        "@/*": ["src/*"],
                        "@components/*": ["src/components/*"]
                    }
                }
            }"#,
        )
        .unwrap();

        let cfg = TsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.base_url.unwrap(), "");
        assert_eq!(cfg.paths.len(), 2);
        assert_eq!(cfg.paths["@/*"], vec!["src/*"]);
    }

    #[test]
    fn test_load_with_extends() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("base.json"),
            r#"{
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": { "@/*": ["src/*"] }
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{
                "extends": "./base.json",
                "compilerOptions": {
                    "paths": { "@app/*": ["src/app/*"] }
                }
            }"#,
        )
        .unwrap();

        let cfg = TsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.base_url.unwrap(), "");
        // Child paths override parent paths entirely (TS behavior)
        assert_eq!(cfg.paths.len(), 1);
        assert!(cfg.paths.contains_key("@app/*"));
    }

    #[test]
    fn test_load_jsconfig_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("jsconfig.json"),
            r#"{
                "compilerOptions": {
                    "baseUrl": "src"
                }
            }"#,
        )
        .unwrap();

        let cfg = TsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.base_url.unwrap(), "src");
        assert!(cfg.paths.is_empty());
    }

    #[test]
    fn test_resolve_specifier() {
        let cfg = TsConfig {
            base_url: Some("src".to_string()),
            paths: {
                let mut m = HashMap::new();
                m.insert("@/*".to_string(), vec!["*".to_string()]);
                m
            },
        };

        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/components/Foo.ts", 1i64);

        let try_resolve = |candidate: &str, p2id: &HashMap<&str, i64>| -> Option<i64> {
            p2id.get(candidate).copied()
        };

        let result = cfg.resolve_specifier("@/components/Foo.ts", &path_to_id, &try_resolve);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_base_url_without_paths() {
        let cfg = TsConfig {
            base_url: Some("src".to_string()),
            paths: HashMap::new(),
        };

        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/utils/helper.ts", 42i64);

        let try_resolve = |candidate: &str, p2id: &HashMap<&str, i64>| -> Option<i64> {
            p2id.get(candidate).copied()
        };

        let result = cfg.resolve_specifier("utils/helper.ts", &path_to_id, &try_resolve);
        assert_eq!(result, Some(42));
    }
}
