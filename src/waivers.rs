use crate::constraints::ConstraintFinding;
use crate::db::{ConstraintWaiverRow, HealthFindingRow, HealthWaiverRow};

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

    fn find_waiver(&self, waivers: &[ConstraintWaiverRow]) -> Option<WaiverMeta> {
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

pub struct ResolvedHealthFinding {
    pub finding: HealthFindingRow,
    pub file_path: String,
    pub symbol_name: Option<String>,
}

impl Waivable for ResolvedHealthFinding {
    type WaiverSet = [HealthWaiverRow];

    fn find_waiver(&self, waivers: &[HealthWaiverRow]) -> Option<WaiverMeta> {
        waivers
            .iter()
            .find(|w| {
                w.biomarker_kind == self.finding.biomarker_kind
                    && w.file_path == self.file_path
                    && match &w.symbol_qualified_name {
                        None => true,
                        Some(wname) => self.symbol_name.as_deref() == Some(wname.as_str()),
                    }
            })
            .map(|w| WaiverMeta {
                rationale: w.rationale.clone(),
                waived_by: w.waived_by.clone(),
            })
    }
}
