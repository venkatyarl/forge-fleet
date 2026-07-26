use serde::{Deserialize, Serialize};

/// A project registered with ForgeFleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Project {
    pub id: String,
    pub display_name: Option<String>,
    pub status: String,
    pub workstream_id: Option<String>,
    pub digest_template_id: Option<String>,
    pub logo_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::Project;

    #[test]
    fn optional_project_extensions_deserialize_when_absent() {
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": "forge-fleet",
            "display_name": "ForgeFleet",
            "status": "active"
        }))
        .expect("deserialize project");

        assert_eq!(project.digest_template_id, None);
        assert_eq!(project.workstream_id, None);
        assert_eq!(project.logo_url, None);
    }
}
