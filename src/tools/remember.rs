use std::collections::HashSet;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, ResolveResult};
use crate::error::{Result, SutraError};
use crate::lessons::{AnchorKind, HashResolver, LessonsDb, StoreLessonParams};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// The lesson text — what you learned that a future editor needs to know.
    /// Required when storing a new lesson; ignored when citing.
    #[serde(default)]
    pub text: Option<String>,
    /// Location anchors: symbol names or file paths this lesson applies to.
    /// Required when storing a new lesson; ignored when citing.
    #[serde(default)]
    pub location_anchors: Option<Vec<LocationAnchor>>,
    /// Yojana task IDs that motivated this lesson (e.g. ["sutra/38"]).
    /// For cite mode, the first entry is recorded as the citing task.
    #[serde(default)]
    pub source_tasks: Option<Vec<String>>,
    /// Project slug where the lesson was discovered
    #[serde(default)]
    pub project_origin: Option<String>,
    /// Category tags (e.g. ["rust", "sqlite", "concurrency"]).
    ///
    /// A tag naming a language marks the lesson as language-specific and keeps
    /// it out of workspaces in other languages. Common names and shorthands
    /// ("python", "py", "golang") are recognised automatically; prefix anything
    /// else with `lang:` (e.g. "lang:nim") to make the claim explicit. All other
    /// tags are topic tags and surface in every workspace.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Cite an existing lesson by ID. Records a citation and increases confidence.
    /// When confidence crosses the threshold, the lesson becomes verified.
    #[serde(default)]
    pub cite: Option<String>,
    /// When true alongside `cite`, flags the lesson as wrong/outdated (decreases confidence).
    /// Does not delete — the lesson may be corrected rather than removed.
    #[serde(default)]
    pub anti_verify: Option<bool>,
    /// Workspace for anchor enrichment. When provided, sutra auto-generates
    /// import-pattern anchors, directory anchors, and categories from the workspace graph.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// A location anchor, accepted in two forms:
/// - a bare string (`"_ExploreAppState"`, `"lib/main.dart"`) — `kind` is
///   inferred: values containing `/` or ending in a file extension are treated
///   as files, everything else as symbols;
/// - an explicit object (`{"kind": "symbol", "value": "X"}`) for full control,
///   including the `import_pattern` and `directory` kinds.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum LocationAnchor {
    /// Bare symbol name or file path; `kind` is inferred.
    Shorthand(String),
    /// Explicit anchor with kind and value.
    Explicit {
        /// Anchor type: "symbol", "file", "import_pattern", or "directory"
        kind: String,
        /// The symbol name or file path
        value: String,
    },
}

impl LocationAnchor {
    /// The anchor value (symbol name or path), regardless of form.
    fn value(&self) -> &str {
        match self {
            LocationAnchor::Shorthand(s) => s,
            LocationAnchor::Explicit { value, .. } => value,
        }
    }

    /// The anchor kind: explicit when given, otherwise inferred from the value.
    fn kind_str(&self) -> &str {
        match self {
            LocationAnchor::Explicit { kind, .. } => kind,
            LocationAnchor::Shorthand(s) if looks_like_file(s) => "file",
            LocationAnchor::Shorthand(_) => "symbol",
        }
    }
}

/// Heuristic for bare-string anchors: a path separator or a trailing
/// `.<ext>` (1–5 alphanumeric chars) marks a file. A `::` marks a qualified
/// symbol, never a file.
fn looks_like_file(s: &str) -> bool {
    if s.contains("::") {
        return false;
    }
    if s.contains('/') {
        return true;
    }
    matches!(
        s.rsplit_once('.'),
        Some((stem, ext))
            if !stem.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
    )
}

pub fn handle(
    lessons_db: &LessonsDb,
    workspace_db: Option<&Db>,
    args: &RememberArgs,
) -> Result<serde_json::Value> {
    if let Some(lesson_id) = &args.cite {
        if args.anti_verify.unwrap_or(false) {
            let result = lessons_db.anti_verify(lesson_id)?;
            return Ok(json!({
                "anti_verified": true,
                "lesson_id": lesson_id,
                "new_confidence": result.new_confidence,
                "verified": result.verified,
            }));
        }
        let task_id = args
            .source_tasks
            .as_ref()
            .and_then(|t| t.first())
            .map(|s| s.as_str());
        let resolver = workspace_db.map(|db| build_hash_resolver(db));
        let result = lessons_db.cite(lesson_id, task_id, resolver.as_ref().map(|r| r.as_ref()))?;
        return Ok(json!({
            "cited": true,
            "lesson_id": lesson_id,
            "new_confidence": result.new_confidence,
            "verified": result.verified,
            "crossed_threshold": result.crossed_threshold,
        }));
    }

    let text = args
        .text
        .as_deref()
        .ok_or_else(|| SutraError::InvalidArgument {
            tool: "sutra_remember",
            argument: "text",
            constraint: "required when storing a new lesson (omit `cite` or provide `text`)".into(),
            received: None,
            next_action: "Provide the lesson text and retry.".into(),
        })?;
    let anchors_raw =
        args.location_anchors
            .as_deref()
            .ok_or_else(|| SutraError::InvalidArgument {
                tool: "sutra_remember",
                argument: "location_anchors",
                constraint: "required when storing a new lesson".into(),
                received: None,
                next_action: "Provide at least one location anchor and retry.".into(),
            })?;

    let anchors: Vec<(AnchorKind, &str)> = anchors_raw
        .iter()
        .map(|a| {
            let kind = match a.kind_str() {
                "symbol" => AnchorKind::Symbol,
                "file" => AnchorKind::File,
                "import_pattern" => AnchorKind::ImportPattern,
                "directory" => AnchorKind::Directory,
                other => {
                    return Err(SutraError::InvalidArgument {
                        tool: "sutra_remember",
                        argument: "location_anchors[].kind",
                        constraint: "must be one of: symbol, file, import_pattern, directory"
                            .into(),
                        received: Some(other.to_string()),
                        next_action: "Fix the anchor kind and retry.".into(),
                    });
                }
            };
            Ok((kind, a.value()))
        })
        .collect::<Result<Vec<_>>>()?;

    let cats: Vec<&str> = args
        .categories
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| s.as_str())
        .collect();
    let tasks: Vec<&str> = args
        .source_tasks
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| s.as_str())
        .collect();

    let enrichment = workspace_db.map(|db| enrich(db, &anchors));

    let mut all_anchors: Vec<(AnchorKind, &str)> = anchors;
    let mut all_cats: Vec<&str> = cats;

    let enriched_anchors;
    let enriched_cats;
    if let Some(e) = &enrichment {
        enriched_anchors = &e.anchors;
        enriched_cats = &e.categories;
        for (k, v) in enriched_anchors {
            if !all_anchors
                .iter()
                .any(|(ek, ev)| ek == k && *ev == v.as_str())
            {
                all_anchors.push((*k, v.as_str()));
            }
        }
        for c in enriched_cats {
            if !all_cats.contains(&c.as_str()) {
                all_cats.push(c.as_str());
            }
        }
    }

    let total_anchors = all_anchors.len();
    let id = lessons_db.store(&StoreLessonParams {
        text,
        anchors: &all_anchors,
        categories: &all_cats,
        source_task_ids: &tasks,
        project_origin: args.project_origin.as_deref(),
    })?;

    let pruned = if let Some(db) = workspace_db {
        let freq = db.import_root_file_counts().unwrap_or_default();
        lessons_db
            .prune_high_freq_import_anchors(&freq, IMPORT_FREQ_CAP)
            .unwrap_or(0)
    } else {
        0
    };

    Ok(json!({
        "stored": true,
        "lesson_id": id,
        "anchor_count": total_anchors,
        "enriched": enrichment.is_some(),
        "pruned_anchors": pruned,
    }))
}

// ---------------------------------------------------------------------------
// Enrichment
// ---------------------------------------------------------------------------

const IMPORT_TECH_MAP: &[(&str, &str)] = &[
    ("rusqlite", "sqlite"),
    ("sqlx", "sql"),
    ("tokio", "async"),
    ("serde", "serialization"),
    ("serde_json", "json"),
    ("flutter", "flutter"),
    ("http", "http"),
    ("hyper", "http"),
    ("reqwest", "http"),
    ("tonic", "grpc"),
    ("prost", "grpc"),
    ("tracing", "observability"),
];

struct Enrichment {
    anchors: Vec<(AnchorKind, String)>,
    categories: Vec<String>,
}

const IMPORT_FREQ_CAP: usize = 5;

fn enrich(db: &Db, explicit_anchors: &[(AnchorKind, &str)]) -> Enrichment {
    let mut seen_dirs: HashSet<String> = HashSet::new();
    let mut seen_imports: HashSet<String> = HashSet::new();
    let mut cats: HashSet<String> = HashSet::new();
    let mut anchors = Vec::new();

    let freq = db.import_root_file_counts().unwrap_or_default();

    for &(kind, value) in explicit_anchors {
        let file_row = match kind {
            AnchorKind::Symbol => match db.resolve_symbol_diagnostic(value, None) {
                Ok(ResolveResult::Unique(sym)) => db.file_by_id(sym.file_id).ok().flatten(),
                _ => None,
            },
            AnchorKind::File => db.file_by_path(value).ok().flatten(),
            _ => continue,
        };

        let Some(file) = file_row else { continue };

        if let Some((dir, _)) = file.path.rsplit_once('/')
            && dir.contains('/')
            && seen_dirs.insert(dir.to_string())
        {
            anchors.push((AnchorKind::Directory, dir.to_string()));
        }

        let lang = file.language.to_lowercase();
        if !lang.is_empty() {
            cats.insert(lang);
        }

        let imports = db.imports_for_file(file.id).unwrap_or_default();
        for imp in &imports {
            let root = import_root(&imp.imported_path);
            if !root.is_empty() && seen_imports.insert(root.to_string()) {
                if let Some(&(_, tech)) = IMPORT_TECH_MAP.iter().find(|&&(k, _)| k == root) {
                    cats.insert(tech.to_string());
                }
                if freq.get(root).copied().unwrap_or(0) <= IMPORT_FREQ_CAP {
                    anchors.push((AnchorKind::ImportPattern, format!("{root}::*")));
                }
            }
        }
    }

    Enrichment {
        anchors,
        categories: cats.into_iter().collect(),
    }
}

pub fn build_hash_resolver(db: &Db) -> Box<HashResolver<'_>> {
    Box::new(move |kind: &str, value: &str| -> Option<String> {
        match kind {
            "symbol" => {
                let sym = match db.resolve_symbol_diagnostic(value, None) {
                    Ok(ResolveResult::Unique(s)) => s,
                    _ => return None,
                };
                db.file_by_id(sym.file_id)
                    .ok()
                    .flatten()
                    .map(|f| f.content_hash)
            }
            "file" => db
                .file_by_path(value)
                .ok()
                .flatten()
                .map(|f| f.content_hash),
            _ => None,
        }
    })
}

fn import_root(imported_path: &str) -> &str {
    if let Some(rest) = imported_path.strip_prefix("package:") {
        return rest.split('/').next().unwrap_or("");
    }
    if imported_path.starts_with("dart:") || imported_path.starts_with("pub use ") {
        return "";
    }
    let root = imported_path.split("::").next().unwrap_or("");
    if root.starts_with('.') || root.starts_with('/') {
        return "";
    }
    match root {
        "crate" | "self" | "super" | "std" => "",
        _ => root,
    }
}

#[cfg(test)]
mod tests {
    use super::IMPORT_TECH_MAP;
    use crate::lessons::{LANG_TAG_PREFIX, normalize_category};

    /// Enrichment writes technology tags automatically, so a technology name
    /// that `normalize_category` reads as a language claim is not a mislabel —
    /// it silently scopes the lesson to a language no workspace reports, and it
    /// surfaces nowhere. `sqlx` -> `sql` is the live instance: `sql` was briefly
    /// in KNOWN_LANGUAGES, which would have buried every lesson anchored to
    /// sqlx-importing code.
    #[test]
    fn known_languages_do_not_claim_tech_tags() {
        for &(import, tech) in IMPORT_TECH_MAP {
            let normalized = normalize_category(tech);
            assert!(
                !normalized.starts_with(LANG_TAG_PREFIX),
                "tech tag `{tech}` (from import `{import}`) normalizes to `{normalized}`, \
                 which scopes the lesson to a language instead of a topic — drop it from \
                 KNOWN_LANGUAGES/LANGUAGE_ALIASES"
            );
        }
    }
}
