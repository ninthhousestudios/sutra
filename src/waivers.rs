use crate::constraints::ConstraintFinding;
use crate::db::ConstraintWaiverRow;

#[derive(Debug, Clone, PartialEq)]
pub struct WaiverMeta {
    pub rationale: String,
    pub waived_by: String,
}

#[derive(Debug, Clone)]
pub struct Waived<F> {
    pub finding: F,
    pub rationale: String,
    pub waived_by: String,
}

/// `waived_by` of a finding waived by an in-place `justify` comment rather than
/// an `accepted.toml` entry.
pub const JUSTIFIED_BY: &str = "justify-comment";

pub trait Waivable: Sized {
    type WaiverSet: ?Sized;
    fn find_waiver(&self, waivers: &Self::WaiverSet) -> Option<WaiverMeta>;
}

pub fn partition<F: Waivable>(
    findings: Vec<F>,
    waivers: &F::WaiverSet,
) -> (Vec<F>, Vec<Waived<F>>) {
    let mut active = Vec::new();
    let mut waived = Vec::new();
    for f in findings {
        match f.find_waiver(waivers) {
            Some(meta) => waived.push(Waived {
                finding: f,
                rationale: meta.rationale,
                waived_by: meta.waived_by,
            }),
            None => active.push(f),
        }
    }
    (active, waived)
}

impl Waivable for ConstraintFinding {
    type WaiverSet = [ConstraintWaiverRow];

    /// An in-place justification (a rule's `justify` marker) waives the match
    /// before any `accepted.toml` waiver is consulted: its reason lives next to
    /// the code, so it is the rationale the report should show.
    fn find_waiver(&self, waivers: &[ConstraintWaiverRow]) -> Option<WaiverMeta> {
        if let Some(reason) = &self.justification {
            return Some(WaiverMeta {
                rationale: reason.clone(),
                waived_by: JUSTIFIED_BY.to_string(),
            });
        }
        waivers
            .iter()
            .find(|w| {
                w.constraint_id == self.constraint_id
                    && w.file_path == self.from_path
                    && match &w.symbol_qualified_name {
                        None => true,
                        Some(wsym) => self.enclosing_symbol.as_deref() == Some(wsym.as_str()),
                    }
            })
            .map(|w| WaiverMeta {
                rationale: w.rationale.clone(),
                waived_by: w.waived_by.clone(),
            })
    }
}
