use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConventionsArgs {
    pub workspace: String,
    /// Action: "list", "accept", "dismiss", "set_lifecycle", "waive", "list_waivers", "revoke_waiver"
    pub action: String,
    /// Proposal ID (for accept/dismiss)
    #[serde(default)]
    pub proposal_id: Option<i64>,
    /// Convention ID (for set_lifecycle, waive, list_waivers)
    #[serde(default)]
    pub convention_id: Option<String>,
    /// Target lifecycle state: descriptive, preferred, deprecated, forbidden (for set_lifecycle)
    #[serde(default)]
    pub lifecycle_state: Option<String>,
    /// Reason for lifecycle change (for set_lifecycle)
    #[serde(default)]
    pub reason: Option<String>,
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

const VALID_STATES: &[&str] = &["descriptive", "preferred", "deprecated", "forbidden"];

pub fn handle(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    match args.action.as_str() {
        "list" => handle_list(db),
        "accept" => handle_accept(db, args),
        "dismiss" => handle_dismiss(db, args),
        "set_lifecycle" => handle_set_lifecycle(db, args),
        "waive" => handle_waive(db, args),
        "list_waivers" => handle_list_waivers(db, args),
        "revoke_waiver" => handle_revoke_waiver(db, args),
        other => Err(SutraError::Internal(format!(
            "unknown action: {other}. expected: list, accept, dismiss, set_lifecycle, waive, list_waivers, revoke_waiver"
        ))),
    }
}

fn handle_list(db: &Db) -> Result<serde_json::Value> {
    let conventions = db.all_conventions_merged()?;
    let proposals = db.pending_proposals()?;

    let conventions_out: Vec<_> = conventions
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "antecedent": c.antecedent,
                "consequent": c.consequent,
                "support": c.support,
                "confidence": c.confidence,
                "lifecycle_state": c.lifecycle_state.as_deref().unwrap_or("descriptive"),
                "component_id": c.component_id,
                "first_seen": c.first_seen,
                "last_seen": c.last_seen,
            })
        })
        .collect();

    let proposals_out: Vec<_> = proposals
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "convention_id": p.convention_id,
                "proposed_transition": p.proposed_transition,
                "signal_rationale": p.signal_rationale,
                "created_at": p.created_at,
            })
        })
        .collect();

    Ok(json!({
        "conventions": conventions_out,
        "pending_proposals": proposals_out,
    }))
}

fn handle_accept(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    let proposal_id = args
        .proposal_id
        .ok_or_else(|| SutraError::Internal("accept requires proposal_id".into()))?;

    let proposal = db
        .get_proposal(proposal_id)?
        .ok_or_else(|| SutraError::Internal(format!("proposal {proposal_id} not found")))?;

    if proposal.status != "pending" {
        return Err(SutraError::Internal(format!(
            "proposal {proposal_id} is already {}",
            proposal.status
        )));
    }

    let target_state = proposal
        .proposed_transition
        .split('\u{2192}')
        .last()
        .unwrap_or("preferred");

    if target_state == "deleted" {
        db.set_convention_lifecycle(
            &proposal.convention_id,
            "deprecated",
            Some("deletion accepted — convention retained as deprecated"),
        )?;
    } else {
        db.set_convention_lifecycle(
            &proposal.convention_id,
            target_state,
            Some(&format!("accepted proposal {proposal_id}")),
        )?;
    }

    db.resolve_proposal(proposal_id, "accepted")?;

    Ok(json!({
        "accepted": proposal_id,
        "convention_id": proposal.convention_id,
        "new_state": if target_state == "deleted" { "deprecated" } else { target_state },
    }))
}

fn handle_dismiss(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    let proposal_id = args
        .proposal_id
        .ok_or_else(|| SutraError::Internal("dismiss requires proposal_id".into()))?;

    let proposal = db
        .get_proposal(proposal_id)?
        .ok_or_else(|| SutraError::Internal(format!("proposal {proposal_id} not found")))?;

    if proposal.status != "pending" {
        return Err(SutraError::Internal(format!(
            "proposal {proposal_id} is already {}",
            proposal.status
        )));
    }

    db.resolve_proposal(proposal_id, "dismissed")?;

    Ok(json!({
        "dismissed": proposal_id,
        "convention_id": proposal.convention_id,
    }))
}

fn handle_set_lifecycle(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    let convention_id = args
        .convention_id
        .as_ref()
        .ok_or_else(|| SutraError::Internal("set_lifecycle requires convention_id".into()))?;

    let state = args
        .lifecycle_state
        .as_ref()
        .ok_or_else(|| SutraError::Internal("set_lifecycle requires lifecycle_state".into()))?;

    if !VALID_STATES.contains(&state.as_str()) {
        return Err(SutraError::Internal(format!(
            "invalid lifecycle_state: {state}. expected one of: {}",
            VALID_STATES.join(", ")
        )));
    }

    db.set_convention_lifecycle(convention_id, state, args.reason.as_deref())?;

    Ok(json!({
        "convention_id": convention_id,
        "lifecycle_state": state,
    }))
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
