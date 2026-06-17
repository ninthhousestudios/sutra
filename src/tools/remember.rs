use std::collections::HashSet;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};
use crate::lessons::{AnchorKind, LessonsDb, StoreLessonParams};

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
    /// Category tags (e.g. ["rust", "sqlite", "concurrency"])
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LocationAnchor {
    /// Anchor type: "symbol" or "file"
    pub kind: String,
    /// The symbol name or file path
    pub value: String,
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
        let result = lessons_db.cite(lesson_id, task_id)?;
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
            let kind = match a.kind.as_str() {
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
            Ok((kind, a.value.as_str()))
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
            if !all_cats.iter().any(|&ec| ec == c.as_str()) {
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

    Ok(json!({
        "stored": true,
        "lesson_id": id,
        "anchor_count": total_anchors,
        "enriched": enrichment.is_some(),
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

fn enrich(db: &Db, explicit_anchors: &[(AnchorKind, &str)]) -> Enrichment {
    let mut seen_dirs: HashSet<String> = HashSet::new();
    let mut seen_imports: HashSet<String> = HashSet::new();
    let mut cats: HashSet<String> = HashSet::new();
    let mut anchors = Vec::new();

    for &(kind, value) in explicit_anchors {
        let file_row = match kind {
            AnchorKind::Symbol => db
                .resolve_symbol(value, None)
                .ok()
                .flatten()
                .and_then(|sym| db.file_by_id(sym.file_id).ok().flatten()),
            AnchorKind::File => db.file_by_path(value).ok().flatten(),
            _ => continue,
        };

        let Some(file) = file_row else { continue };

        if let Some((dir, _)) = file.path.rsplit_once('/') {
            if seen_dirs.insert(dir.to_string()) {
                anchors.push((AnchorKind::Directory, dir.to_string()));
            }
        }

        let lang = file.language.to_lowercase();
        if !lang.is_empty() {
            cats.insert(lang);
        }

        let imports = db.imports_for_file(file.id).unwrap_or_default();
        for imp in &imports {
            let root = import_root(&imp.imported_path);
            if !root.is_empty() && seen_imports.insert(root.to_string()) {
                anchors.push((AnchorKind::ImportPattern, format!("{root}::*")));
                if let Some(&(_, tech)) = IMPORT_TECH_MAP.iter().find(|&&(k, _)| k == root) {
                    cats.insert(tech.to_string());
                }
            }
        }
    }

    Enrichment {
        anchors,
        categories: cats.into_iter().collect(),
    }
}

fn import_root(imported_path: &str) -> &str {
    if let Some(rest) = imported_path.strip_prefix("package:") {
        return rest.split('/').next().unwrap_or("");
    }
    // Dart stdlib — not useful as a technology anchor
    if imported_path.starts_with("dart:") {
        return "";
    }
    // Rust: "rusqlite::params" → "rusqlite"; relative paths → skip
    let root = imported_path.split("::").next().unwrap_or("");
    if root.starts_with('.') || root.starts_with('/') {
        return "";
    }
    root
}
