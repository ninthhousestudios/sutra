use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::error::{Result, SutraError};
use crate::lessons::{AnchorKind, LessonsDb, StoreLessonParams};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// The lesson text — what you learned that a future editor needs to know
    pub text: String,
    /// Location anchors: symbol names or file paths this lesson applies to
    pub location_anchors: Vec<LocationAnchor>,
    /// Yojana task IDs that motivated this lesson (e.g. ["sutra/38"])
    #[serde(default)]
    pub source_tasks: Option<Vec<String>>,
    /// Project slug where the lesson was discovered
    #[serde(default)]
    pub project_origin: Option<String>,
    /// Category tags (e.g. ["rust", "sqlite", "concurrency"])
    #[serde(default)]
    pub categories: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LocationAnchor {
    /// Anchor type: "symbol" or "file"
    pub kind: String,
    /// The symbol name or file path
    pub value: String,
}

pub fn handle(lessons_db: &LessonsDb, args: &RememberArgs) -> Result<serde_json::Value> {
    let anchors: Vec<(AnchorKind, &str)> = args
        .location_anchors
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

    let id = lessons_db.store(&StoreLessonParams {
        text: &args.text,
        anchors: &anchors,
        categories: &cats,
        source_task_ids: &tasks,
        project_origin: args.project_origin.as_deref(),
    })?;

    Ok(json!({
        "stored": true,
        "lesson_id": id,
        "anchor_count": args.location_anchors.len(),
    }))
}
