use anyhow::{Result, bail};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiraRepoBinding {
    pub repo_id: Uuid,
    pub repo_url: String,
}

pub fn repo_binding_allowed(
    jira_parent: bool,
    repo_id: Option<Uuid>,
    canonical_project_repo: bool,
    parent_repo_id: Option<Uuid>,
    allowed_repo_ids: &[Uuid],
) -> bool {
    if !jira_parent {
        return true;
    }
    let Some(repo_id) = repo_id else {
        return false;
    };
    canonical_project_repo
        && if allowed_repo_ids.len() > 1 {
            allowed_repo_ids.contains(&repo_id)
        } else {
            parent_repo_id == Some(repo_id)
        }
}

fn metadata_repo_ids(metadata: &Value) -> Vec<Uuid> {
    ["allowed_repo_ids", "repo_ids", "repo_binding_repo_ids"]
        .into_iter()
        .filter_map(|key| metadata.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(|value| value.as_str().and_then(|raw| Uuid::parse_str(raw).ok()))
        .collect()
}

/// Fail-closed canonical repository check for Jira parents and their children.
///
/// Non-Jira work deliberately remains compatible with the legacy project-repo
/// fallback. Jira work must carry a `repo_id` that still belongs to its project;
/// a child must additionally remain inside its Jira parent's declared repo set.
pub async fn validate_jira_repo_binding(
    pg: &PgPool,
    work_item_id: Uuid,
) -> Result<Option<JiraRepoBinding>> {
    let row = sqlx::query(
        "SELECT w.kind, w.project_id, w.repo_id, NULLIF(w.repo_url, '') AS repo_url, \
                COALESCE(w.metadata, '{}'::jsonb) AS metadata, \
                p.kind AS parent_kind, p.repo_id AS parent_repo_id, \
                COALESCE(p.metadata, '{}'::jsonb) AS parent_metadata \
           FROM work_items w LEFT JOIN work_items p ON p.id = w.parent_id \
          WHERE w.id = $1",
    )
    .bind(work_item_id)
    .fetch_optional(pg)
    .await?;
    let Some(row) = row else {
        bail!("work_item {work_item_id} no longer exists");
    };
    let kind: String = row.get("kind");
    let parent_kind: Option<String> = row.try_get("parent_kind").ok().flatten();
    if kind != "jira" && parent_kind.as_deref() != Some("jira") {
        return Ok(None);
    }

    let repo_id: Option<Uuid> = row.try_get("repo_id").ok().flatten();
    let Some(repo_id) = repo_id else {
        bail!("Jira work_item {work_item_id} has no canonical repo_id binding");
    };
    let project_id: String = row.get("project_id");
    let canonical: Option<String> = sqlx::query_scalar(
        "SELECT github_url FROM project_repos \
          WHERE id = $1 AND project_id = $2 AND NULLIF(github_url, '') IS NOT NULL",
    )
    .bind(repo_id)
    .bind(&project_id)
    .fetch_optional(pg)
    .await?;
    let Some(repo_url) = canonical else {
        bail!(
            "Jira work_item {work_item_id} repo_id {repo_id} is not an allowed repo for project {project_id}"
        );
    };
    let stored_url: Option<String> = row.try_get("repo_url").ok().flatten();
    if stored_url.as_deref().is_some_and(|url| url != repo_url) {
        bail!("Jira work_item {work_item_id} repo_url does not match canonical project_repos URL");
    }

    if parent_kind.as_deref() == Some("jira") {
        let parent_repo_id: Option<Uuid> = row.try_get("parent_repo_id").ok().flatten();
        let parent_metadata: Value = row.get("parent_metadata");
        let allowed = metadata_repo_ids(&parent_metadata);
        if allowed.len() > 1 {
            if !allowed.contains(&repo_id) {
                bail!("Jira child {work_item_id} is outside its parent's allowed repo set");
            }
        } else if let Some(parent_repo_id) = parent_repo_id {
            if repo_id != parent_repo_id {
                bail!("Jira child {work_item_id} does not inherit its parent's canonical repo");
            }
        } else {
            bail!("Jira parent has no resolved canonical or multi-repo binding");
        }
    }

    Ok(Some(JiraRepoBinding { repo_id, repo_url }))
}

pub fn jira_multi_repo_ids(metadata: &Value) -> Vec<Uuid> {
    metadata_repo_ids(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_explicit_multi_repo_ids() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let value = serde_json::json!({"allowed_repo_ids": [a, b, "not-a-uuid"]});
        assert_eq!(jira_multi_repo_ids(&value), vec![a, b]);
        assert!(jira_multi_repo_ids(&serde_json::json!({"is_primary": true})).is_empty());
    }

    #[test]
    fn unique_jira_binding_succeeds_and_wrong_repo_fails() {
        let expected = Uuid::new_v4();
        assert!(repo_binding_allowed(
            true,
            Some(expected),
            true,
            Some(expected),
            &[]
        ));
        assert!(!repo_binding_allowed(
            true,
            Some(Uuid::new_v4()),
            true,
            Some(expected),
            &[]
        ));
        assert!(!repo_binding_allowed(true, None, true, Some(expected), &[]));
    }

    #[test]
    fn multi_repo_children_stay_inside_allowed_set() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert!(repo_binding_allowed(true, Some(a), true, None, &[a, b]));
        assert!(repo_binding_allowed(true, Some(b), true, None, &[a, b]));
        assert!(!repo_binding_allowed(
            true,
            Some(Uuid::new_v4()),
            true,
            None,
            &[a, b]
        ));
    }

    #[test]
    fn non_jira_behavior_is_unchanged() {
        assert!(repo_binding_allowed(false, None, false, None, &[]));
    }
}
