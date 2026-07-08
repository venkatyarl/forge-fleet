pub struct FleetSecrets {
    pub pr_automerge_mode: bool,
}

impl Default for FleetSecrets {
    fn default() -> Self {
        Self {
            pr_automerge_mode: false,
        }
    }
}
