use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::error::Result;
use crate::lessons::{LessonsDb, LessonsSearchParams};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LessonsArgs {
    /// Search query. Matches lesson text (FTS5) and category tags — phrase it
    /// as the work you are about to do ("sqlite migration", "golden testing").
    #[serde(default)]
    pub query: Option<String>,
    /// Filter by category tag
    #[serde(default)]
    pub category: Option<String>,
    /// Filter by anchor symbol name
    #[serde(default)]
    pub symbol: Option<String>,
    /// Filter to lessons anchored to this exact file path — the retrieval path
    /// for the file-anchored advisory the edit guard emits.
    #[serde(default)]
    pub file: Option<String>,
    /// Filter to verified lessons only
    #[serde(default)]
    pub verified: Option<bool>,
    /// Project scope
    #[serde(default)]
    pub project: Option<String>,
    /// Include archived lessons in results (default: false)
    #[serde(default)]
    pub include_archived: Option<bool>,
}

pub fn handle(lessons_db: &LessonsDb, args: &LessonsArgs) -> Result<serde_json::Value> {
    let params = LessonsSearchParams {
        query: args.query.as_deref(),
        category: args.category.as_deref(),
        symbol: args.symbol.as_deref(),
        file: args.file.as_deref(),
        verified: args.verified,
        project: args.project.as_deref(),
        include_archived: args.include_archived.unwrap_or(false),
        // Kept tight: the category tier can fill the tail with topic matches,
        // and each lesson is prose-heavy — a 50-row default returned ~14k
        // tokens of mostly-tangential hits (sutra/331).
        limit: 15,
    };
    let lessons = lessons_db.search(&params)?;
    Ok(json!({
        "total": lessons.len(),
        "lessons": lessons,
    }))
}
