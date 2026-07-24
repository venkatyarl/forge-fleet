// Self-improvement module implementation
use crate::ff_agent::self_improvement::{Council, Research, Evidence};
use crate::ff_core::obsidian_export::{ff_interactions_has_project_id};
use crate::ff_mc::operational_api::list_work_items_from_store;
use crate::ff_db::queries::pg_complete_parent_work_items;

pub struct Council {
    pub council: Vec<String>,
    pub charter_prompt: String,
}

impl Council {
    pub fn new() -> Self {
        Council {
            council: vec![
                "codex".to_string(),
                "kimi".to_string(),
            ],
            charter_prompt: "given this evidence, propose the 3 highest-impact improvements to <subsystem>".to_string(),
        }
    }
}

impl Research {
    pub fn new() -> Self {
        Research {
            pub research: Vec<String>,
        }
    }
}

impl Evidence {
    pub fn new() -> Self {
        Evidence {
            pub evidence: Vec<String>,
        }
    }
}

impl SelfImprove {
    pub fn new() -> Self {
        SelfImprove {
            council: Council::new(),
            research: Research::new(),
            evidence: Evidence::new(),
        }
    }
}
