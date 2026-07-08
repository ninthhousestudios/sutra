use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConventionsArgs {
    pub workspace: String,
    /// Action: "list", "waive", "list_waivers", "revoke_waiver"
    pub action: String,
    /// Convention ID (for waive, list_waivers)
    #[serde(default)]
    pub convention_id: Option<String>,
    /// Symbol qualified name, e.g. "process" or "src/foo.rs::process" (for waive)
    #[serde(default)]
    pub symbol: Option<String>,
    /// Component ID to scope the waiver (for waive; empty string if unscoped)
    #[serde(default)]
    pub component_id: Option<String>,
    /// Rationale for the waiver (for waive)
    #[serde(default)]
    pub rationale: Option<String>,
    /// Who is granting the waiver (for waive)
    #[serde(default)]
    pub waived_by: Option<String>,
    /// Waiver ID (for revoke_waiver)
    #[serde(default)]
    pub waiver_id: Option<i64>,
}

pub fn handle(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    match args.action.as_str() {
        "list" => handle_list(db),
        "waive" => handle_waive(db, args),
        "list_waivers" => handle_list_waivers(db, args),
        "revoke_waiver" => handle_revoke_waiver(db, args),
        other => Err(SutraError::Internal(format!(
            "unknown action: {other}. expected: list, waive, list_waivers, revoke_waiver"
        ))),
    }
}

fn handle_list(db: &Db) -> Result<serde_json::Value> {
    let conventions = db.all_conventions()?;

    let conventions_out: Vec<_> = conventions
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "antecedent": c.antecedent,
                "consequent": c.consequent,
                "support": c.support,
                "confidence": c.confidence,
                "component_id": c.component_id,
            })
        })
        .collect();

    Ok(json!({ "conventions": conventions_out }))
}

fn handle_waive(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    let convention_id = args
        .convention_id
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires convention_id".into()))?;
    let symbol = args
        .symbol
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires symbol".into()))?;
    let rationale = args
        .rationale
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires rationale".into()))?;
    let waived_by = args
        .waived_by
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires waived_by".into()))?;
    let component_id = args.component_id.as_deref().unwrap_or("");

    let id = db.create_waiver(convention_id, symbol, component_id, rationale, waived_by)?;

    Ok(json!({
        "waiver_id": id,
        "convention_id": convention_id,
        "symbol": symbol,
        "component_id": component_id,
        "rationale": rationale,
        "waived_by": waived_by,
    }))
}

fn handle_list_waivers(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    let waivers = db.list_waivers(args.convention_id.as_deref())?;

    let waivers_out: Vec<_> = waivers
        .iter()
        .map(|w| {
            json!({
                "id": w.id,
                "convention_id": w.convention_id,
                "symbol": w.symbol_qualified_name,
                "component_id": w.component_id,
                "rationale": w.rationale,
                "waived_by": w.waived_by,
                "waived_at": w.waived_at,
            })
        })
        .collect();

    Ok(json!({ "waivers": waivers_out }))
}

fn handle_revoke_waiver(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    let waiver_id = args
        .waiver_id
        .ok_or_else(|| SutraError::Internal("revoke_waiver requires waiver_id".into()))?;

    let revoked = db.revoke_waiver(waiver_id)?;

    if !revoked {
        return Err(SutraError::Internal(format!(
            "waiver {waiver_id} not found"
        )));
    }

    Ok(json!({ "revoked": waiver_id }))
}
