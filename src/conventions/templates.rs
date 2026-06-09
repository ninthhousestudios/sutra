use std::collections::HashMap;

use crate::conventions::engine::{Convention, SymbolAttrs};
use crate::db::Db;
use crate::error::Result;

const MAX_EXEMPLARS: usize = 3;

#[derive(Debug, Clone)]
pub struct SymbolSignatureInfo {
    pub signature: Option<String>,
    pub visibility: Option<String>,
    pub language_attrs: Option<String>,
    pub cognitive: Option<i64>,
}

#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
struct SignatureElements {
    visibility: Option<String>,
    is_async: bool,
    is_unsafe: bool,
    name: String,
    generics: Option<String>,
    params: Vec<String>,
    return_type: Option<String>,
}

fn decompose_signature(
    signature: &str,
    visibility: Option<&str>,
    language_attrs: Option<&str>,
) -> Option<SignatureElements> {
    let sig = signature.strip_prefix("fn ")?.trim();

    let (name, rest) = split_at_first_delimiter(sig)?;
    let rest = rest.trim();

    let (generics, rest) = if rest.starts_with('<') {
        extract_balanced(rest, '<', '>')?
    } else {
        (None, rest)
    };

    let rest = rest.trim();
    if !rest.starts_with('(') {
        return None;
    }
    let (params_str, rest) = extract_balanced(rest, '(', ')')?;
    let params = split_params(params_str.as_deref().unwrap_or(""));

    let rest = rest.trim();
    let return_type = if let Some(ret) = rest.strip_prefix("->") {
        let ret = ret.trim();
        if ret.is_empty() {
            None
        } else {
            Some(ret.to_string())
        }
    } else {
        None
    };

    let (is_async, is_unsafe) = parse_modifier_flags(language_attrs);

    Some(SignatureElements {
        visibility: visibility.map(|v| v.to_string()),
        is_async,
        is_unsafe,
        name: name.to_string(),
        generics,
        params,
        return_type,
    })
}

fn split_at_first_delimiter(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(['(', '<'])?;
    Some((&s[..idx], &s[idx..]))
}

fn extract_balanced(s: &str, open: char, close: char) -> Option<(Option<String>, &str)> {
    if !s.starts_with(open) {
        return Some((None, s));
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                let inner = &s[open.len_utf8()..i];
                let inner = if inner.is_empty() {
                    None
                } else {
                    Some(inner.to_string())
                };
                return Some((inner, &s[i + close.len_utf8()..]));
            }
        }
    }
    None
}

fn split_params(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    let mut params = Vec::new();
    let mut depth_angle = 0i32;
    let mut depth_paren = 0i32;
    let mut start = 0;

    for (i, c) in s.char_indices() {
        match c {
            '<' => depth_angle += 1,
            '>' => depth_angle -= 1,
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            ',' if depth_angle == 0 && depth_paren == 0 => {
                let param = s[start..i].trim();
                if !param.is_empty() {
                    params.push(param.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        params.push(last.to_string());
    }
    params
}

fn parse_modifier_flags(language_attrs: Option<&str>) -> (bool, bool) {
    let Some(json_str) = language_attrs else {
        return (false, false);
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, serde_json::Value>>(json_str) else {
        return (false, false);
    };
    let is_async = map
        .get("is_async")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let is_unsafe = map
        .get("is_unsafe")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (is_async, is_unsafe)
}

fn select_exemplars(
    convention: &Convention,
    sym_attrs: &[SymbolAttrs],
    sig_info: &HashMap<String, SymbolSignatureInfo>,
) -> Vec<String> {
    let required: std::collections::HashSet<&str> = convention
        .antecedent
        .iter()
        .chain(convention.consequent.iter())
        .map(|s| s.as_str())
        .collect();

    let mut candidates: Vec<(&str, usize, i64, usize)> = Vec::new();

    for (idx, sa) in sym_attrs.iter().enumerate() {
        if !required
            .iter()
            .all(|r| sa.attributes.iter().any(|a| a == r))
        {
            continue;
        }
        if let Some(ref comp_id) = convention.component_id
            && sa.component_id.as_deref() != Some(comp_id)
        {
            continue;
        }
        let info = sig_info.get(&sa.name);
        if info.is_none_or(|i| i.signature.is_none()) {
            continue;
        }
        let coverage = sa.attributes.len() - required.len();
        let cognitive = info.map_or(0, |i| i.cognitive.unwrap_or(0));
        candidates.push((sa.name.as_str(), coverage, cognitive, idx));
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    let complexities: Vec<i64> = candidates.iter().map(|c| c.2).collect();
    let mut sorted_c = complexities.clone();
    sorted_c.sort();
    let median = sorted_c[sorted_c.len() / 2];

    candidates.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                let dist_a = (a.2 - median).unsigned_abs();
                let dist_b = (b.2 - median).unsigned_abs();
                dist_a.cmp(&dist_b)
            })
            .then_with(|| b.3.cmp(&a.3))
    });

    candidates
        .iter()
        .take(MAX_EXEMPLARS)
        .map(|c| c.0.to_string())
        .collect()
}

fn generate_template(exemplars: &[SignatureElements]) -> String {
    if exemplars.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();

    let all_same_vis = exemplars
        .windows(2)
        .all(|w| w[0].visibility == w[1].visibility);
    if all_same_vis && let Some(ref vis) = exemplars[0].visibility {
        parts.push(vis.clone());
    }

    if exemplars.iter().all(|e| e.is_async) {
        parts.push("async".into());
    }
    if exemplars.iter().all(|e| e.is_unsafe) {
        parts.push("unsafe".into());
    }

    parts.push("fn".into());
    parts.push("$NAME".into());

    let template_so_far = parts.join(" ");

    let generics_part = if exemplars.iter().all(|e| e.generics.is_none()) {
        String::new()
    } else if exemplars.windows(2).all(|w| w[0].generics == w[1].generics) {
        if let Some(ref g) = exemplars[0].generics {
            format!("<{g}>")
        } else {
            String::new()
        }
    } else {
        "<$GENERICS>".into()
    };

    let params_part = build_params_template(exemplars);

    let return_part = build_return_template(exemplars);

    let mut result = template_so_far;
    result.push_str(&generics_part);
    result.push_str(&params_part);
    if !return_part.is_empty() {
        result.push_str(" -> ");
        result.push_str(&return_part);
    }

    result
}

fn build_params_template(exemplars: &[SignatureElements]) -> String {
    if exemplars.iter().all(|e| e.params.is_empty()) {
        return "()".into();
    }

    let has_self: Vec<Option<&str>> = exemplars
        .iter()
        .map(|e| {
            e.params.first().and_then(|p| {
                let trimmed = p.trim();
                if trimmed == "&self" || trimmed == "&mut self" || trimmed == "self" {
                    Some(trimmed)
                } else {
                    None
                }
            })
        })
        .collect();

    let all_have_self = has_self.iter().all(|s| s.is_some());
    let same_self = all_have_self && has_self.windows(2).all(|w| w[0] == w[1]);

    if same_self {
        let self_param = has_self[0].unwrap();
        let remaining: Vec<Vec<&str>> = exemplars
            .iter()
            .map(|e| e.params[1..].iter().map(|p| p.as_str()).collect())
            .collect();
        if remaining.iter().all(|r| r.is_empty()) {
            return format!("({self_param})");
        }
        let all_same_remaining = remaining.windows(2).all(|w| w[0] == w[1]);
        if all_same_remaining {
            let rest = remaining[0].join(", ");
            return format!("({self_param}, {rest})");
        }
        return format!("({self_param}, $PARAMS)");
    }

    let all_same_params = exemplars.windows(2).all(|w| w[0].params == w[1].params);
    if all_same_params {
        let params = exemplars[0].params.join(", ");
        return format!("({params})");
    }

    "($PARAMS)".into()
}

fn build_return_template(exemplars: &[SignatureElements]) -> String {
    let returns: Vec<Option<&str>> = exemplars.iter().map(|e| e.return_type.as_deref()).collect();

    if returns.iter().all(|r| r.is_none()) {
        return String::new();
    }

    if returns.windows(2).all(|w| w[0] == w[1]) {
        return returns[0].unwrap_or("").to_string();
    }

    let wrapper = detect_common_wrapper(&returns);
    if let Some(w) = wrapper {
        return format!("{w}<$T>");
    }

    "$RETURN".into()
}

fn detect_common_wrapper(returns: &[Option<&str>]) -> Option<&'static str> {
    let wrappers = ["Result", "Option", "Vec", "Box"];
    wrappers
        .iter()
        .find(|&w| {
            returns
                .iter()
                .all(|r| r.is_some_and(|r| r.starts_with(w) && r[w.len()..].starts_with('<')))
        })
        .map(|v| v as _)
}

pub fn generate_templates_for_conventions(
    conventions: &[Convention],
    sym_attrs: &[SymbolAttrs],
    sig_info: &HashMap<String, SymbolSignatureInfo>,
    db: &Db,
) -> Result<usize> {
    let mut count = 0;
    for conv in conventions {
        if conv.support < 3 {
            continue;
        }
        let exemplar_names = select_exemplars(conv, sym_attrs, sig_info);
        if exemplar_names.len() < 2 {
            continue;
        }

        let elements: Vec<SignatureElements> = exemplar_names
            .iter()
            .filter_map(|name| {
                let info = sig_info.get(name)?;
                decompose_signature(
                    info.signature.as_deref()?,
                    info.visibility.as_deref(),
                    info.language_attrs.as_deref(),
                )
            })
            .collect();

        if elements.len() < 2 {
            continue;
        }

        let template_text = generate_template(&elements);
        if template_text.is_empty() {
            continue;
        }

        db.upsert_convention_template(&conv.id, &template_text, &exemplar_names)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_basic_fn() {
        let el = decompose_signature(
            "fn parse_config(path: &Path) -> Result<Config>",
            Some("pub"),
            None,
        )
        .unwrap();
        assert_eq!(el.name, "parse_config");
        assert_eq!(el.visibility.as_deref(), Some("pub"));
        assert_eq!(el.params, vec!["path: &Path"]);
        assert_eq!(el.return_type.as_deref(), Some("Result<Config>"));
        assert!(!el.is_async);
        assert!(el.generics.is_none());
    }

    #[test]
    fn decompose_method_with_self() {
        let el = decompose_signature(
            "fn process(&mut self, data: Vec<u8>) -> Result<()>",
            Some("pub"),
            Some(r#"{"is_async": true}"#),
        )
        .unwrap();
        assert_eq!(el.name, "process");
        assert_eq!(el.params, vec!["&mut self", "data: Vec<u8>"]);
        assert_eq!(el.return_type.as_deref(), Some("Result<()>"));
        assert!(el.is_async);
    }

    #[test]
    fn decompose_with_generics() {
        let el = decompose_signature(
            "fn convert<T: Into<String>>(value: T) -> String",
            None,
            None,
        )
        .unwrap();
        assert_eq!(el.name, "convert");
        assert_eq!(el.generics.as_deref(), Some("T: Into<String>"));
        assert_eq!(el.params, vec!["value: T"]);
        assert_eq!(el.return_type.as_deref(), Some("String"));
    }

    #[test]
    fn decompose_no_params_no_return() {
        let el = decompose_signature("fn init()", None, None).unwrap();
        assert_eq!(el.name, "init");
        assert!(el.params.is_empty());
        assert!(el.return_type.is_none());
    }

    #[test]
    fn decompose_complex_params_with_nested_generics() {
        let el = decompose_signature(
            "fn query(items: &[HashMap<String, Vec<u8>>], limit: usize) -> Vec<Row>",
            Some("pub(crate)"),
            None,
        )
        .unwrap();
        assert_eq!(el.name, "query");
        assert_eq!(el.params.len(), 2);
        assert_eq!(el.params[0], "items: &[HashMap<String, Vec<u8>>]");
        assert_eq!(el.params[1], "limit: usize");
        assert_eq!(el.return_type.as_deref(), Some("Vec<Row>"));
    }

    #[test]
    fn exemplar_selection_ranks_by_coverage_then_complexity() {
        let conv = Convention {
            id: "c1".into(),
            antecedent: vec!["kind:function".into(), "vis:pub".into()],
            consequent: vec!["has_doc".into()],
            support: 5,
            confidence: 0.95,
            component_id: None,
        };

        let sym_attrs = vec![
            SymbolAttrs {
                name: "high_coverage".into(),
                file: "a.rs".into(),
                attributes: vec![
                    "kind:function".into(),
                    "vis:pub".into(),
                    "has_doc".into(),
                    "returns_result".into(),
                    "is_async".into(),
                ],
                component_id: None,
            },
            SymbolAttrs {
                name: "low_coverage".into(),
                file: "b.rs".into(),
                attributes: vec!["kind:function".into(), "vis:pub".into(), "has_doc".into()],
                component_id: None,
            },
            SymbolAttrs {
                name: "med_coverage".into(),
                file: "c.rs".into(),
                attributes: vec![
                    "kind:function".into(),
                    "vis:pub".into(),
                    "has_doc".into(),
                    "returns_result".into(),
                ],
                component_id: None,
            },
        ];

        let mut sig_info = HashMap::new();
        for name in ["high_coverage", "low_coverage", "med_coverage"] {
            sig_info.insert(
                name.to_string(),
                SymbolSignatureInfo {
                    signature: Some("fn x() -> Result<()>".into()),
                    visibility: Some("pub".into()),
                    language_attrs: None,
                    cognitive: Some(5),
                },
            );
        }

        let exemplars = select_exemplars(&conv, &sym_attrs, &sig_info);
        assert_eq!(exemplars[0], "high_coverage");
        assert_eq!(exemplars[1], "med_coverage");
        assert_eq!(exemplars[2], "low_coverage");
    }

    #[test]
    fn exemplar_selection_skips_symbols_without_signature() {
        let conv = Convention {
            id: "c1".into(),
            antecedent: vec!["kind:function".into()],
            consequent: vec!["vis:pub".into()],
            support: 3,
            confidence: 0.9,
            component_id: None,
        };

        let sym_attrs = vec![SymbolAttrs {
            name: "no_sig".into(),
            file: "a.rs".into(),
            attributes: vec!["kind:function".into(), "vis:pub".into()],
            component_id: None,
        }];

        let mut sig_info = HashMap::new();
        sig_info.insert(
            "no_sig".to_string(),
            SymbolSignatureInfo {
                signature: None,
                visibility: Some("pub".into()),
                language_attrs: None,
                cognitive: Some(3),
            },
        );

        let exemplars = select_exemplars(&conv, &sym_attrs, &sig_info);
        assert!(exemplars.is_empty());
    }

    #[test]
    fn template_common_result_return() {
        let exemplars = vec![
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "parse".into(),
                generics: None,
                params: vec!["path: &Path".into()],
                return_type: Some("Result<Config>".into()),
            },
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "load".into(),
                generics: None,
                params: vec!["id: u64".into()],
                return_type: Some("Result<Data>".into()),
            },
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "read".into(),
                generics: None,
                params: vec!["name: &str".into()],
                return_type: Some("Result<String>".into()),
            },
        ];

        let template = generate_template(&exemplars);
        assert_eq!(template, "pub fn $NAME($PARAMS) -> Result<$T>");
    }

    #[test]
    fn template_preserves_self_param() {
        let exemplars = vec![
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "process".into(),
                generics: None,
                params: vec!["&self".into(), "data: Vec<u8>".into()],
                return_type: Some("Result<()>".into()),
            },
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "execute".into(),
                generics: None,
                params: vec!["&self".into(), "cmd: &str".into()],
                return_type: Some("Result<Output>".into()),
            },
        ];

        let template = generate_template(&exemplars);
        assert_eq!(template, "pub fn $NAME(&self, $PARAMS) -> Result<$T>");
    }

    #[test]
    fn template_includes_async() {
        let exemplars = vec![
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: true,
                is_unsafe: false,
                name: "fetch".into(),
                generics: None,
                params: vec!["url: &str".into()],
                return_type: Some("Result<Response>".into()),
            },
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: true,
                is_unsafe: false,
                name: "send".into(),
                generics: None,
                params: vec!["msg: Message".into()],
                return_type: Some("Result<()>".into()),
            },
        ];

        let template = generate_template(&exemplars);
        assert_eq!(template, "pub async fn $NAME($PARAMS) -> Result<$T>");
    }

    #[test]
    fn template_mixed_return_types() {
        let exemplars = vec![
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "count".into(),
                generics: None,
                params: vec![],
                return_type: Some("usize".into()),
            },
            SignatureElements {
                visibility: Some("pub".into()),
                is_async: false,
                is_unsafe: false,
                name: "name".into(),
                generics: None,
                params: vec![],
                return_type: Some("String".into()),
            },
        ];

        let template = generate_template(&exemplars);
        assert_eq!(template, "pub fn $NAME() -> $RETURN");
    }

    #[test]
    fn exemplar_selection_respects_component_scope() {
        let conv = Convention {
            id: "c1".into(),
            antecedent: vec!["kind:function".into()],
            consequent: vec!["vis:pub".into()],
            support: 3,
            confidence: 0.9,
            component_id: Some("comp_a".into()),
        };

        let sym_attrs = vec![
            SymbolAttrs {
                name: "in_comp".into(),
                file: "a.rs".into(),
                attributes: vec!["kind:function".into(), "vis:pub".into()],
                component_id: Some("comp_a".into()),
            },
            SymbolAttrs {
                name: "wrong_comp".into(),
                file: "b.rs".into(),
                attributes: vec!["kind:function".into(), "vis:pub".into()],
                component_id: Some("comp_b".into()),
            },
        ];

        let mut sig_info = HashMap::new();
        for name in ["in_comp", "wrong_comp"] {
            sig_info.insert(
                name.to_string(),
                SymbolSignatureInfo {
                    signature: Some("fn x()".into()),
                    visibility: Some("pub".into()),
                    language_attrs: None,
                    cognitive: Some(3),
                },
            );
        }

        let exemplars = select_exemplars(&conv, &sym_attrs, &sig_info);
        assert_eq!(exemplars, vec!["in_comp"]);
    }
}
