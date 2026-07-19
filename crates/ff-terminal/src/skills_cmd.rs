//! `ff skills` — manage the V105 skills catalog: list / show / sync /
//! remove / retire / materialize / import-from-repo.
//!
//! Source of truth: Postgres `skills` table. Disk under
//! `~/.forgefleet/skills/<source>/<family>/<name>/SKILL.md` is a
//! materialized cache that the runtime skill_catalog.rs reads at
//! session start.

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use ff_agent::skills_db;
use std::path::PathBuf;

#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommand {
    /// List every skill currently in the DB (one row per skill).
    List {
        /// Filter by source (e.g. anthropic, wshobson, forgefleet, forgefleet-legacy).
        #[arg(long)]
        source: Option<String>,
        /// Filter by family (e.g. design, code, docs).
        #[arg(long)]
        family: Option<String>,
        /// Emit a JSON array (one lean metadata object per skill) for
        /// scripts/agents instead of the table. Omits the SKILL.md body.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the SKILL.md for one skill.
    Show {
        /// Skill name as it appears in the DB (the `name` column).
        name: String,
        /// Source — disambiguates skills with the same name across imports.
        #[arg(long, default_value = "forgefleet")]
        source: String,
    },
    /// Re-render every DB row onto disk under `~/.forgefleet/skills/`
    /// and (optionally) garbage-collect on-disk files that no longer have
    /// a DB row.
    Sync {
        /// After materialize, remove on-disk skill directories that no
        /// longer have a matching DB row.
        #[arg(long, default_value_t = false)]
        prune: bool,
        /// Show what sync would write/prune without touching disk.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Remove a skill from the DB and disk. Does NOT add it to
    /// retired_skills — use `retire` for that.
    Remove {
        name: String,
        #[arg(long)]
        source: String,
    },
    /// Mark a skill retired so future syncs won't re-import it. Removes
    /// the current rows from `skills` and inserts into `retired_skills`.
    Retire {
        name: String,
        #[arg(long)]
        source: String,
        #[arg(long, default_value = "operator retired")]
        reason: String,
    },
    /// Import every SKILL.md under a local directory tree into the DB.
    /// Use this after `git clone`-ing a skills repo locally.
    Import {
        /// Path to the local directory tree containing SKILL.md files.
        path: PathBuf,
        /// Source identifier to record on each row (e.g. anthropic,
        /// wshobson, forgefleet, microsoft, clawhub).
        #[arg(long)]
        source: String,
        /// Optional upstream URL recorded on the row.
        #[arg(long)]
        source_url: Option<String>,
        /// Override the family — useful when a repo doesn't encode a
        /// family directory layout. Otherwise inferred from the path.
        #[arg(long)]
        family: Option<String>,
    },
    /// Clone a public git repo to a temp dir and import its SKILL.md
    /// files in one shot.
    ImportRepo {
        /// HTTPS or SSH git URL (e.g. https://github.com/anthropics/skills).
        url: String,
        /// Source identifier (defaults to the github owner from the URL).
        #[arg(long)]
        source: Option<String>,
        /// Override family (otherwise inferred from path).
        #[arg(long)]
        family: Option<String>,
        /// Branch or tag to check out (default: repo default branch).
        #[arg(long)]
        r#ref: Option<String>,
    },
    /// Show count / source breakdown / risk distribution.
    Stats {
        /// Emit the counts as a JSON object (total + by_source / by_family
        /// / by_risk maps) for scripts/agents instead of the text report.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn handle_skills(cmd: SkillsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow!("connect Postgres: {e}"))?;

    match cmd {
        SkillsCommand::List {
            source,
            family,
            json,
        } => list_cmd(&pool, source, family, json).await,
        SkillsCommand::Show { name, source } => show_cmd(&pool, &name, &source).await,
        SkillsCommand::Sync { prune, dry_run } => sync_cmd(&pool, prune, dry_run).await,
        SkillsCommand::Remove { name, source } => remove_cmd(&pool, &source, &name).await,
        SkillsCommand::Retire {
            name,
            source,
            reason,
        } => retire_cmd(&pool, &source, &name, &reason).await,
        SkillsCommand::Import {
            path,
            source,
            source_url,
            family,
        } => {
            import_cmd(
                &pool,
                &path,
                &source,
                source_url.as_deref(),
                family.as_deref(),
            )
            .await
        }
        SkillsCommand::ImportRepo {
            url,
            source,
            family,
            r#ref,
        } => {
            import_repo_cmd(
                &pool,
                &url,
                source.as_deref(),
                family.as_deref(),
                r#ref.as_deref(),
            )
            .await
        }
        SkillsCommand::Stats { json } => stats_cmd(&pool, json).await,
    }
}

async fn list_cmd(
    pool: &sqlx::PgPool,
    source: Option<String>,
    family: Option<String>,
    json: bool,
) -> Result<()> {
    let all = skills_db::list_all(pool).await?;
    let mut filtered: Vec<_> = all
        .into_iter()
        .filter(|s| match &source {
            Some(src) => s.source == *src,
            None => true,
        })
        .filter(|s| match &family {
            Some(f) => s.family.as_deref() == Some(f.as_str()),
            None => true,
        })
        .collect();
    filtered.sort_by(|a, b| {
        (
            a.source.as_str(),
            a.family.as_deref().unwrap_or(""),
            a.name.as_str(),
        )
            .cmp(&(
                b.source.as_str(),
                b.family.as_deref().unwrap_or(""),
                b.name.as_str(),
            ))
    });

    if json {
        // Lean per-skill metadata — deliberately omits the full SKILL.md
        // body (use `ff skills show` for that). description is the full
        // first line, not the table's truncated form.
        let arr: Vec<serde_json::Value> = filtered.iter().map(skill_to_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    println!(
        "{:<22} {:<14} {:<14} {:<8} {:<8} DESCRIPTION",
        "NAME", "SOURCE", "FAMILY", "VERSION", "RISK"
    );
    for s in &filtered {
        let first_line = s
            .description
            .as_deref()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        // Char-safe truncation — byte-slicing panics on multi-byte UTF-8
        // (open-design ships skills with CJK descriptions). truncate() counts
        // chars and appends the ellipsis itself.
        let desc = truncate(first_line, 60);
        println!(
            "{:<22} {:<14} {:<14} {:<8} {:<8} {}",
            truncate(&s.name, 22),
            truncate(&s.source, 14),
            truncate(s.family.as_deref().unwrap_or("-"), 14),
            truncate(&s.version, 8),
            truncate(&s.risk_level, 8),
            desc
        );
    }
    println!();
    println!("{} skills", filtered.len());
    Ok(())
}

async fn show_cmd(pool: &sqlx::PgPool, name: &str, source: &str) -> Result<()> {
    let Some(s) = skills_db::get_by_name_source(pool, name, source).await? else {
        return Err(anyhow!("no skill named '{name}' from source '{source}'"));
    };
    println!("# {} / {}", s.source, s.name);
    println!("id:          {}", s.id);
    println!("version:     {}", s.version);
    println!("family:      {}", s.family.as_deref().unwrap_or("-"));
    println!("risk:        {}", s.risk_level);
    if let Some(url) = &s.source_url {
        println!("source_url:  {url}");
    }
    println!("body_sha256: {}", s.body_sha256);
    println!();
    println!("--- SKILL.md ---");
    println!("{}", s.body_md);
    Ok(())
}

async fn sync_cmd(pool: &sqlx::PgPool, prune: bool, dry_run: bool) -> Result<()> {
    let root = skills_db::skills_root();
    if dry_run {
        // Preview only — touch nothing on disk. `materialize_all` would
        // (over)write one SKILL.md per DB row; `--prune` would delete
        // on-disk dirs with no matching row (the destructive part).
        let known = skills_db::list_all(pool).await?;
        println!(
            "[dry-run] would materialize {} skill(s) under {}",
            known.len(),
            root.display()
        );
        if prune {
            let orphans = skills_db::prune_orphans_plan(&known)?;
            if orphans.is_empty() {
                println!("[dry-run] would prune 0 orphaned on-disk skill dir(s)");
            } else {
                println!(
                    "[dry-run] would prune {} orphaned on-disk skill dir(s):",
                    orphans.len()
                );
                for dir in &orphans {
                    println!("  - {}", dir.display());
                }
            }
        } else {
            println!("[dry-run] prune not requested (pass --prune to preview GC)");
        }
        return Ok(());
    }

    let (written, skipped) = skills_db::materialize_all(pool).await?;
    println!(
        "materialized: {written} skill(s); skipped: {skipped}; root={}",
        root.display()
    );
    if prune {
        let known = skills_db::list_all(pool).await?;
        let removed = skills_db::prune_orphans(&known)?;
        println!("pruned {removed} orphaned on-disk skill dir(s)");
    }
    Ok(())
}

async fn remove_cmd(pool: &sqlx::PgPool, source: &str, name: &str) -> Result<()> {
    let n = skills_db::remove_skill(pool, source, name).await?;
    println!("removed {n} row(s) for {source}/{name}");
    Ok(())
}

async fn retire_cmd(pool: &sqlx::PgPool, source: &str, name: &str, reason: &str) -> Result<()> {
    skills_db::retire_skill(pool, source, name, reason, None).await?;
    println!("retired {source}/{name}: {reason}");
    Ok(())
}

async fn import_cmd(
    pool: &sqlx::PgPool,
    path: &std::path::Path,
    source: &str,
    source_url: Option<&str>,
    family: Option<&str>,
) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("path does not exist: {}", path.display()));
    }
    let (imported, updated, skipped_retired, errors) =
        skills_db::import_repo_skills(pool, path, source, source_url, family).await?;
    println!(
        "import summary: imported={imported} updated={updated} skipped_retired={skipped_retired} errors={errors}"
    );

    // Re-materialize after import so disk reflects the latest DB state.
    let (written, _skipped) = skills_db::materialize_all(pool).await?;
    println!(
        "materialized: {written} skill(s) under {}",
        skills_db::skills_root().display()
    );
    Ok(())
}

async fn import_repo_cmd(
    pool: &sqlx::PgPool,
    url: &str,
    source: Option<&str>,
    family: Option<&str>,
    git_ref: Option<&str>,
) -> Result<()> {
    let source = source
        .map(|s| s.to_string())
        .unwrap_or_else(|| derive_source_from_url(url));
    let tmp = tempfile::tempdir().context("create temp dir for git clone")?;
    let dest = tmp.path().join("repo");

    println!("git clone {url} → {}", dest.display());
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    if let Some(r) = git_ref {
        cmd.args(["--branch", r]);
    }
    cmd.arg(url).arg(&dest);
    let status = cmd.status().context("spawn git")?;
    if !status.success() {
        return Err(anyhow!("git clone failed for {url}"));
    }

    let (imported, updated, skipped_retired, errors) =
        skills_db::import_repo_skills(pool, &dest, &source, Some(url), family).await?;
    println!(
        "import summary: imported={imported} updated={updated} skipped_retired={skipped_retired} errors={errors}"
    );

    let (written, _skipped) = skills_db::materialize_all(pool).await?;
    println!(
        "materialized: {written} skill(s) under {}",
        skills_db::skills_root().display()
    );
    Ok(())
}

async fn stats_cmd(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let rows = skills_db::list_all(pool).await?;
    let total = rows.len();
    let mut by_source: std::collections::BTreeMap<String, usize> = Default::default();
    let mut by_family: std::collections::BTreeMap<String, usize> = Default::default();
    let mut by_risk: std::collections::BTreeMap<String, usize> = Default::default();
    for r in &rows {
        *by_source.entry(r.source.clone()).or_default() += 1;
        *by_family
            .entry(r.family.clone().unwrap_or_else(|| "uncategorized".into()))
            .or_default() += 1;
        *by_risk.entry(r.risk_level.clone()).or_default() += 1;
    }

    if json {
        let out = serde_json::json!({
            "total": total,
            "by_source": by_source,
            "by_family": by_family,
            "by_risk": by_risk,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("total skills: {total}");
    println!();
    println!("by source:");
    for (k, v) in &by_source {
        println!("  {k:<20} {v}");
    }
    println!();
    println!("by family:");
    for (k, v) in &by_family {
        println!("  {k:<20} {v}");
    }
    println!();
    println!("by risk:");
    for (k, v) in &by_risk {
        println!("  {k:<20} {v}");
    }
    Ok(())
}

/// Lean per-skill JSON metadata for `ff skills list --json`. Deliberately
/// omits the full SKILL.md body (`body_md`) — agents use `ff skills show`
/// for that — and emits the FULL first line of the description (not the
/// table's truncated form) so the structured output is lossless.
fn skill_to_json(s: &skills_db::SkillRow) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "name": s.name,
        "source": s.source,
        "family": s.family,
        "version": s.version,
        "risk_level": s.risk_level,
        "description": s.description.as_deref().unwrap_or("").lines().next().unwrap_or(""),
        "source_url": s.source_url,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn derive_source_from_url(url: &str) -> String {
    // Best-effort: extract the github owner from the URL path
    // https://github.com/anthropics/skills(.git) → anthropics
    // git@github.com:anthropics/skills.git → anthropics
    let cleaned = url.trim_end_matches(".git");
    let after_host = if let Some(idx) = cleaned.find("github.com") {
        let s = &cleaned[idx + "github.com".len()..];
        s.trim_start_matches(['/', ':'].as_ref())
    } else {
        cleaned
    };
    after_host
        .split('/')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_agent::skills_db::SkillRow;

    fn row(name: &str, desc: Option<&str>, family: Option<&str>) -> SkillRow {
        SkillRow {
            id: uuid::Uuid::nil(),
            name: name.to_string(),
            source: "anthropic".to_string(),
            source_url: Some("https://example/skill".to_string()),
            version: "1.0.0".to_string(),
            family: family.map(|f| f.to_string()),
            description: desc.map(|d| d.to_string()),
            when_to_invoke: None,
            tools: serde_json::Value::Null,
            body_md: "# huge SKILL.md body that must NOT appear in --json".to_string(),
            body_sha256: "deadbeef".to_string(),
            risk_level: "low".to_string(),
            canonical_skill_id: None,
            superseded_by: None,
            combines: serde_json::Value::Null,
        }
    }

    #[test]
    fn skill_json_is_lean_and_omits_body() {
        let v = skill_to_json(&row("docx", Some("Edit Word docs"), Some("docs")));
        let obj = v.as_object().unwrap();
        // Exactly the lean metadata keys — no body_md / body_sha256 / tools.
        assert_eq!(obj.get("name").unwrap(), "docx");
        assert_eq!(obj.get("source").unwrap(), "anthropic");
        assert_eq!(obj.get("family").unwrap(), "docs");
        assert_eq!(obj.get("version").unwrap(), "1.0.0");
        assert_eq!(obj.get("risk_level").unwrap(), "low");
        assert_eq!(obj.get("description").unwrap(), "Edit Word docs");
        assert!(obj.get("body_md").is_none());
        assert!(obj.get("body_sha256").is_none());
        assert!(obj.get("tools").is_none());
    }

    #[test]
    fn skill_json_uses_full_first_line_and_handles_nulls() {
        // description: full first line only (newline-stripped, NOT truncated).
        let long = "This is a long first line that the table would clip with an ellipsis but JSON keeps whole\nsecond line dropped";
        let v = skill_to_json(&row("x", Some(long), None));
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.get("description").unwrap(),
            "This is a long first line that the table would clip with an ellipsis but JSON keeps whole"
        );
        // null family / missing description serialize as JSON null / "".
        assert!(obj.get("family").unwrap().is_null());
        let v2 = skill_to_json(&row("y", None, None));
        assert_eq!(v2.as_object().unwrap().get("description").unwrap(), "");
    }
}
