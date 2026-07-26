use serde::{Deserialize, Serialize};

/// Project fields shared by persistence and API responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub workstream_id: Option<String>,
    pub digest_template_id: Option<String>,
    pub logo_url: Option<String>,
}
