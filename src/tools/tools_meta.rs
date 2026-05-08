use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn handle(
    analysis_enabled: &AtomicBool,
    enable: Option<&[String]>,
    disable: Option<&[String]>,
    list: bool,
) -> serde_json::Value {
    if let Some(tiers) = enable {
        for tier in tiers {
            if tier == "analysis" {
                analysis_enabled.store(true, Ordering::SeqCst);
            }
        }
    }
    if let Some(tiers) = disable {
        for tier in tiers {
            if tier == "analysis" {
                analysis_enabled.store(false, Ordering::SeqCst);
            }
        }
    }

    let analysis_on = analysis_enabled.load(Ordering::SeqCst);

    if list || enable.is_some() || disable.is_some() {
        json!({
            "tiers": {
                "core": {
                    "enabled": true,
                    "tools": ["sutra_health", "sutra_map", "sutra_outline", "sutra_find",
                              "sutra_grep", "sutra_read", "sutra_impact", "sutra_deps",
                              "sutra_parse", "sutra_tools"],
                },
                "analysis": {
                    "enabled": analysis_on,
                    "tools": ["sutra_refs", "sutra_calls", "sutra_diff_impact", "sutra_cochange", "sutra_pr_risk", "sutra_provenance", "sutra_trace"],
                },
            }
        })
    } else {
        json!({
            "tiers": {
                "core": { "enabled": true },
                "analysis": { "enabled": analysis_on },
            }
        })
    }
}
