use anyhow::{Context, Result, ensure};
use ff_db::models::Project;
use sqlx::{PgPool, Postgres, Transaction};

/// Persists projects and maintains their initial standing digest.
pub struct ProjectHandler<'a> {
    pool: &'a PgPool,
}

impl<'a> ProjectHandler<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Insert a project and create its configured initial digest atomically.
    ///
    /// Replaying an insertion is safe: the project is left unchanged and the
    /// digest ensure self-heals if the digest row was removed.
    pub async fn insert(&self, project: &Project) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin project insert")?;

        sqlx::query(
            "INSERT INTO projects \
                (id, display_name, status, workstream_id, digest_template_id, logo_url) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&project.id)
        .bind(&project.display_name)
        .bind(&project.status)
        .bind(&project.workstream_id)
        .bind(&project.digest_template_id)
        .bind(&project.logo_url)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("insert project '{}'", project.id))?;

        if let Some(template_id) = project.digest_template_id.as_deref() {
            ensure_digest(&mut tx, project, template_id).await?;
            verify_or_heal_digest(&mut tx, project, template_id).await?;
        }

        tx.commit().await.context("commit project insert")
    }
}

async fn ensure_digest(
    tx: &mut Transaction<'_, Postgres>,
    project: &Project,
    template_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO project_digest_configs \
            (id, project_id, kind, title, interval_secs, logo_path) \
         VALUES ($1, $2, $3, $4, 900, $5) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(digest_id(&project.id, template_id))
    .bind(&project.id)
    .bind(template_id)
    .bind(
        project
            .display_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&project.id),
    )
    .bind(&project.logo_url)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("create initial digest for project '{}'", project.id))?;
    Ok(())
}

async fn verify_or_heal_digest(
    tx: &mut Transaction<'_, Postgres>,
    project: &Project,
    template_id: &str,
) -> Result<()> {
    if !digest_exists(tx, &project.id, template_id).await? {
        ensure_digest(tx, project, template_id).await?;
    }

    ensure!(
        digest_exists(tx, &project.id, template_id).await?,
        "initial digest missing for project '{}'",
        project.id
    );
    Ok(())
}

async fn digest_exists(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
    template_id: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (\
            SELECT 1 FROM project_digest_configs WHERE id = $1 AND project_id = $2\
         )",
    )
    .bind(digest_id(project_id, template_id))
    .bind(project_id)
    .fetch_one(&mut **tx)
    .await
    .with_context(|| format!("verify initial digest for project '{project_id}'"))
}

fn digest_id(project_id: &str, template_id: &str) -> String {
    format!("{project_id}:{template_id}")
}

#[cfg(test)]
mod tests {
    use super::digest_id;

    #[test]
    fn digest_id_is_stable_per_project_and_template() {
        assert_eq!(digest_id("forge-fleet", "standing"), "forge-fleet:standing");
    }
}
