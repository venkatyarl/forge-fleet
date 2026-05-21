//! Skills (V105) — DB row helpers + on-disk materializer + importer.
//!
//! The Postgres `skills` table is the canonical source of truth for every
//! skill ForgeFleet has imported (anthropics/skills, wshobson/agents,
//! ClawHub picks, our own `forgefleet/` set, legacy `forgefleet-legacy`
//! rows migrated from `software_registry.agent_hint`).
//!
//! Two parallel surfaces use this data:
//!   - **Disk materializer** (`materialize_one` / `materialize_all`)
//!     writes `~/.forgefleet/skills/<source>/<family>/<name>/SKILL.md`
//!     so runtime skill-catalog discovery (skill_catalog.rs) finds them
//!     without a DB roundtrip per session-start.
//!   - **CLI** (`ff skills install / list / show / sync / remove`) and
//!     the importers (`import_repo`) operate against the DB and then
//!     trigger a re-materialize.
//!
//! The skill body on disk is always the canonical YAML-frontmatter form
//! ("name", "description", "when-to-invoke", "tools" in frontmatter,
//! body in markdown). When the DB-stored `body_md` already has
//! frontmatter we use it as-is; otherwise we synthesize it from the
//! structured columns. This keeps `~/.forgefleet/skills/` portable —
//! the same files can be symlinked into `~/.claude/skills/`,
//! `.cursor/skills/`, or fed to OpenCode without translation.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Root for on-disk materialized skills. Every fleet computer can write
/// here — the materializer is idempotent so concurrent writes from a
/// scheduler reconcile cleanly.
pub fn skills_root() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".forgefleet").join("skills")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRow {
    pub id: Uuid,
    pub name: String,
    pub source: String,
    pub source_url: Option<String>,
    pub version: String,
    pub family: Option<String>,
    pub description: Option<String>,
    pub when_to_invoke: Option<String>,
    pub tools: JsonValue,
    pub body_md: String,
    pub body_sha256: String,
    pub risk_level: String,
    pub canonical_skill_id: Option<Uuid>,
    pub superseded_by: Option<Uuid>,
    pub combines: JsonValue,
}

impl SkillRow {
    pub fn disk_path(&self) -> PathBuf {
        let family = self.family.as_deref().unwrap_or("uncategorized");
        skills_root()
            .join(sanitize(&self.source))
            .join(sanitize(family))
            .join(sanitize(&self.name))
            .join("SKILL.md")
    }
}

/// List all skills currently in the DB, sorted by (source, family, name).
pub async fn list_all(pool: &PgPool) -> Result<Vec<SkillRow>> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            JsonValue,
            String,
            String,
            String,
            Option<Uuid>,
            Option<Uuid>,
            JsonValue,
        ),
    >(
        r#"
        SELECT id, name, source, source_url, version, family, description,
               when_to_invoke, tools, body_md, body_sha256, risk_level,
               canonical_skill_id, superseded_by, combines
        FROM skills
        WHERE superseded_by IS NULL
        ORDER BY source, family NULLS LAST, name
        "#,
    )
    .fetch_all(pool)
    .await
    .context("select from skills")?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                name,
                source,
                source_url,
                version,
                family,
                description,
                when_to_invoke,
                tools,
                body_md,
                body_sha256,
                risk_level,
                canonical_skill_id,
                superseded_by,
                combines,
            )| SkillRow {
                id,
                name,
                source,
                source_url,
                version,
                family,
                description,
                when_to_invoke,
                tools,
                body_md,
                body_sha256,
                risk_level,
                canonical_skill_id,
                superseded_by,
                combines,
            },
        )
        .collect())
}

pub async fn get_by_name_source(
    pool: &PgPool,
    name: &str,
    source: &str,
) -> Result<Option<SkillRow>> {
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            JsonValue,
            String,
            String,
            String,
            Option<Uuid>,
            Option<Uuid>,
            JsonValue,
        ),
    >(
        r#"
        SELECT id, name, source, source_url, version, family, description,
               when_to_invoke, tools, body_md, body_sha256, risk_level,
               canonical_skill_id, superseded_by, combines
        FROM skills
        WHERE name = $1 AND source = $2
        ORDER BY installed_at DESC
        LIMIT 1
        "#,
    )
    .bind(name)
    .bind(source)
    .fetch_optional(pool)
    .await
    .context("select skill by name+source")?;

    Ok(row.map(
        |(
            id,
            name,
            source,
            source_url,
            version,
            family,
            description,
            when_to_invoke,
            tools,
            body_md,
            body_sha256,
            risk_level,
            canonical_skill_id,
            superseded_by,
            combines,
        )| SkillRow {
            id,
            name,
            source,
            source_url,
            version,
            family,
            description,
            when_to_invoke,
            tools,
            body_md,
            body_sha256,
            risk_level,
            canonical_skill_id,
            superseded_by,
            combines,
        },
    ))
}

/// Upsert a skill from raw frontmatter+body. If a row with the same
/// (name, source, version) already exists, returns its id without
/// re-inserting. If the same (name, source) exists at a DIFFERENT
/// version, both rows are kept — the older one stays as history. The
/// most-recent installed_at wins for materialize.
pub async fn upsert_skill(
    pool: &PgPool,
    name: &str,
    source: &str,
    source_url: Option<&str>,
    version: &str,
    family: Option<&str>,
    description: Option<&str>,
    when_to_invoke: Option<&str>,
    tools: &JsonValue,
    body_md: &str,
    risk_level: &str,
    security_scan: Option<&JsonValue>,
) -> Result<Uuid> {
    let body_sha = sha256_hex(body_md);

    // Block re-import if (source, name) is retired.
    let retired: Option<(String,)> =
        sqlx::query_as("SELECT retired_reason FROM retired_skills WHERE source = $1 AND name = $2")
            .bind(source)
            .bind(name)
            .fetch_optional(pool)
            .await
            .context("check retired_skills")?;
    if let Some((reason,)) = retired {
        return Err(anyhow!(
            "skill {source}/{name} is retired — reason: {reason}; remove from retired_skills to re-import"
        ));
    }

    let row: (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO skills
            (name, source, source_url, version, family, description,
             when_to_invoke, tools, body_md, body_sha256, risk_level,
             security_scan, installed_at, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now(), now())
        ON CONFLICT (name, source, version) DO UPDATE
            SET source_url     = EXCLUDED.source_url,
                family         = EXCLUDED.family,
                description    = EXCLUDED.description,
                when_to_invoke = EXCLUDED.when_to_invoke,
                tools          = EXCLUDED.tools,
                body_md        = EXCLUDED.body_md,
                body_sha256    = EXCLUDED.body_sha256,
                risk_level     = EXCLUDED.risk_level,
                security_scan  = EXCLUDED.security_scan,
                updated_at     = now()
        RETURNING id
        "#,
    )
    .bind(name)
    .bind(source)
    .bind(source_url)
    .bind(version)
    .bind(family)
    .bind(description)
    .bind(when_to_invoke)
    .bind(tools)
    .bind(body_md)
    .bind(&body_sha)
    .bind(risk_level)
    .bind(security_scan)
    .fetch_one(pool)
    .await
    .context("upsert skill")?;

    Ok(row.0)
}

/// Delete a skill from the DB and (if present) from disk.
pub async fn remove_skill(pool: &PgPool, source: &str, name: &str) -> Result<u64> {
    let rows: Vec<(Uuid, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, source, name, family FROM skills WHERE source = $1 AND name = $2",
    )
    .bind(source)
    .bind(name)
    .fetch_all(pool)
    .await?;

    for (_id, src, nm, fam) in &rows {
        let path = skills_root()
            .join(sanitize(src))
            .join(sanitize(fam.as_deref().unwrap_or("uncategorized")))
            .join(sanitize(nm));
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    let result = sqlx::query("DELETE FROM skills WHERE source = $1 AND name = $2")
        .bind(source)
        .bind(name)
        .execute(pool)
        .await
        .context("delete skill")?;

    Ok(result.rows_affected())
}

/// Mark a skill retired so future syncs won't re-import it. The matching
/// rows in `skills` are deleted; `retired_skills` keeps the (source, name)
/// fingerprint forever.
pub async fn retire_skill(
    pool: &PgPool,
    source: &str,
    name: &str,
    reason: &str,
    superseded_by: Option<Uuid>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO retired_skills (source, name, retired_at, retired_reason, superseded_by)
        VALUES ($1, $2, now(), $3, $4)
        ON CONFLICT (source, name) DO UPDATE
            SET retired_at = EXCLUDED.retired_at,
                retired_reason = EXCLUDED.retired_reason,
                superseded_by  = EXCLUDED.superseded_by
        "#,
    )
    .bind(source)
    .bind(name)
    .bind(reason)
    .bind(superseded_by)
    .execute(pool)
    .await
    .context("insert into retired_skills")?;

    let _ = remove_skill(pool, source, name).await?;
    Ok(())
}

/// Write a single skill to disk as a SKILL.md file with YAML frontmatter
/// + markdown body. Idempotent — if the file on disk already matches the
/// DB body hash, returns Ok without rewriting.
pub fn materialize_one(skill: &SkillRow) -> Result<PathBuf> {
    let path = skill.disk_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if path.exists() {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let existing_body = strip_frontmatter(&existing);
            if sha256_hex(existing_body) == skill.body_sha256 {
                return Ok(path);
            }
        }
    }

    let md = render_skill_md(skill);
    std::fs::write(&path, md).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Materialize every non-superseded skill in the DB. Returns counts.
pub async fn materialize_all(pool: &PgPool) -> Result<(usize, usize)> {
    let rows = list_all(pool).await?;
    let mut written = 0;
    let mut skipped = 0;
    for skill in &rows {
        match materialize_one(skill) {
            Ok(_) => written += 1,
            Err(e) => {
                eprintln!(
                    "warn: materialize {}/{} failed: {e}",
                    skill.source, skill.name
                );
                skipped += 1;
            }
        }
    }
    Ok((written, skipped))
}

/// Remove on-disk files that don't have a matching DB row anymore. Run
/// after `materialize_all` to garbage-collect retired or renamed skills.
pub fn prune_orphans(known: &[SkillRow]) -> Result<usize> {
    let root = skills_root();
    if !root.exists() {
        return Ok(0);
    }
    let mut wanted = std::collections::HashSet::new();
    for s in known {
        wanted.insert(s.disk_path());
    }
    let mut removed = 0;
    for source_dir in walk_dirs(&root)? {
        let skill_md = source_dir.join("SKILL.md");
        if skill_md.exists() && !wanted.contains(&skill_md) {
            let _ = std::fs::remove_dir_all(&source_dir);
            removed += 1;
        }
    }
    Ok(removed)
}

fn walk_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    // Three levels deep: <source>/<family>/<name>/SKILL.md
    let mut out = Vec::new();
    for source_entry in std::fs::read_dir(root)? {
        let source_entry = source_entry?;
        if !source_entry.file_type()?.is_dir() {
            continue;
        }
        for family_entry in std::fs::read_dir(source_entry.path())? {
            let family_entry = family_entry?;
            if !family_entry.file_type()?.is_dir() {
                continue;
            }
            for name_entry in std::fs::read_dir(family_entry.path())? {
                let name_entry = name_entry?;
                if name_entry.file_type()?.is_dir() {
                    out.push(name_entry.path());
                }
            }
        }
    }
    Ok(out)
}

fn render_skill_md(s: &SkillRow) -> String {
    // If the body already starts with YAML frontmatter, write it as-is.
    if s.body_md.trim_start().starts_with("---") {
        return s.body_md.clone();
    }

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", s.name));
    if let Some(desc) = &s.description {
        out.push_str(&format!(
            "description: |\n  {}\n",
            desc.replace('\n', "\n  ")
        ));
    }
    if let Some(when) = &s.when_to_invoke {
        out.push_str(&format!(
            "when-to-invoke: |\n  {}\n",
            when.replace('\n', "\n  ")
        ));
    }
    if let Some(family) = &s.family {
        out.push_str(&format!("family: {family}\n"));
    }
    out.push_str(&format!("source: {}\n", s.source));
    out.push_str(&format!("version: {}\n", s.version));
    if s.risk_level != "medium" {
        out.push_str(&format!("risk-level: {}\n", s.risk_level));
    }
    if let Some(arr) = s.tools.as_array()
        && !arr.is_empty()
    {
        out.push_str("tools:\n");
        for t in arr {
            if let Some(name) = t.as_str() {
                out.push_str(&format!("  - {name}\n"));
            }
        }
    }
    out.push_str("---\n\n");
    out.push_str(&s.body_md);
    out
}

fn strip_frontmatter(s: &str) -> &str {
    let trimmed = s.trim_start_matches(|c: char| c == '\u{feff}');
    if let Some(rest) = trimmed.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        return &rest[end + 5..];
    }
    s
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// ─── Importer ────────────────────────────────────────────────────────────────

/// Import every SKILL.md found at `<repo_dir>/**/SKILL.md` into the DB.
/// `source` is the source identifier we store in the row (e.g.
/// "anthropics", "wshobson", "forgefleet"). `source_url` is optional but
/// recommended (https url to the upstream repo).
///
/// Returns (imported_new, updated_existing, skipped_retired, errors).
pub async fn import_repo_skills(
    pool: &PgPool,
    repo_dir: &Path,
    source: &str,
    source_url: Option<&str>,
    family_override: Option<&str>,
) -> Result<(usize, usize, usize, usize)> {
    let mut imported = 0;
    let mut updated = 0;
    let mut skipped_retired = 0;
    let mut errors = 0;

    let skill_files = find_skill_files(repo_dir)?;
    for path in &skill_files {
        match read_skill_file(path) {
            Ok((fm, body)) => {
                let name = fm.name.clone().unwrap_or_else(|| derive_name(path));
                let family = family_override
                    .map(|s| s.to_string())
                    .or(fm.family.clone())
                    .or_else(|| derive_family_from_path(path, repo_dir));
                let version = fm.version.clone().unwrap_or_else(|| "1.0.0".to_string());
                let tools = JsonValue::Array(
                    fm.tools
                        .iter()
                        .map(|s| JsonValue::String(s.clone()))
                        .collect(),
                );
                let risk = classify_risk(&body, &fm.tools);
                let security_scan = serde_json::json!({
                    "static": static_scan(&body),
                });

                // Was it retired?
                let retired: Option<(String,)> = sqlx::query_as(
                    "SELECT retired_reason FROM retired_skills WHERE source = $1 AND name = $2",
                )
                .bind(source)
                .bind(&name)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
                if retired.is_some() {
                    skipped_retired += 1;
                    continue;
                }

                let prior = get_by_name_source(pool, &name, source).await.ok().flatten();
                match upsert_skill(
                    pool,
                    &name,
                    source,
                    source_url,
                    &version,
                    family.as_deref(),
                    fm.description.as_deref(),
                    fm.when_to_invoke.as_deref(),
                    &tools,
                    &body,
                    &risk,
                    Some(&security_scan),
                )
                .await
                {
                    Ok(_) => {
                        if prior.is_some() {
                            updated += 1;
                        } else {
                            imported += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("warn: upsert {source}/{name} failed: {e}");
                        errors += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("warn: read {} failed: {e}", path.display());
                errors += 1;
            }
        }
    }
    Ok((imported, updated, skipped_retired, errors))
}

fn find_skill_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s == ".git" || s == "node_modules" || s == "target" || s == ".github" {
                    continue;
                }
                stack.push(entry.path());
            } else if ft.is_file() {
                let p = entry.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| {
                        n.eq_ignore_ascii_case("SKILL.md") || n.eq_ignore_ascii_case("skill.md")
                    })
                    .unwrap_or(false)
                {
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    when_to_invoke: Option<String>,
    family: Option<String>,
    version: Option<String>,
    tools: Vec<String>,
}

fn read_skill_file(path: &Path) -> Result<(Frontmatter, String)> {
    let raw = std::fs::read_to_string(path)?;
    let trimmed = raw.trim_start_matches('\u{feff}');
    let mut fm = Frontmatter::default();
    let body: String;
    if let Some(rest) = trimmed.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let fm_text = &rest[..end];
        body = rest[end + 5..].to_string();
        parse_frontmatter_loose(fm_text, &mut fm);
    } else {
        body = raw.clone();
    }
    Ok((fm, body))
}

/// Cheap YAML parser — only handles the subset we expect in SKILL.md
/// frontmatter (key: value, key: |, list items). Avoids a full YAML
/// dep for a few well-defined fields.
fn parse_frontmatter_loose(text: &str, fm: &mut Frontmatter) {
    let mut iter = text.lines().peekable();
    while let Some(line) = iter.next() {
        let line = line.trim_end();
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_lowercase();
            let raw_v = v.trim_start();
            // Block-scalar form: `description: |`
            if raw_v == "|" || raw_v == ">" {
                let mut block = String::new();
                while let Some(next) = iter.peek() {
                    if next.starts_with("  ") {
                        if !block.is_empty() {
                            block.push('\n');
                        }
                        block.push_str(next.trim_start());
                        iter.next();
                    } else if next.trim().is_empty() {
                        iter.next();
                    } else {
                        break;
                    }
                }
                assign_fm(fm, &key, &block);
                continue;
            }
            // List form: `tools:` followed by `- foo`
            if raw_v.is_empty() && key == "tools" {
                while let Some(next) = iter.peek() {
                    let trimmed = next.trim_start();
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        fm.tools.push(item.trim().trim_matches('"').to_string());
                        iter.next();
                    } else if next.trim().is_empty() {
                        iter.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
            let value = raw_v.trim_matches('"').trim_matches('\'').to_string();
            assign_fm(fm, &key, &value);
        }
    }
}

fn assign_fm(fm: &mut Frontmatter, key: &str, value: &str) {
    match key {
        "name" => fm.name = Some(value.to_string()),
        "description" => fm.description = Some(value.to_string()),
        "when-to-invoke" | "when_to_invoke" | "when_to_use" | "when-to-use" => {
            fm.when_to_invoke = Some(value.to_string())
        }
        "family" | "category" | "type" => fm.family = Some(value.to_string()),
        "version" => fm.version = Some(value.to_string()),
        _ => {}
    }
}

fn derive_name(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn derive_family_from_path(path: &Path, repo_root: &Path) -> Option<String> {
    let rel = path.strip_prefix(repo_root).ok()?;
    let comps: Vec<_> = rel.components().collect();
    // <family>/<name>/SKILL.md → comps = [family, name, SKILL.md]
    if comps.len() >= 3 {
        comps
            .first()
            .and_then(|c| c.as_os_str().to_str())
            .map(String::from)
    } else {
        None
    }
}

// ─── Security gate ───────────────────────────────────────────────────────────

/// Cheap static scan — flags shell-injection patterns and obvious
/// credential exfiltration. Not a real sandbox; that's enforced by the
/// runtime (skill execution happens inside the agent's tool sandbox).
fn static_scan(body: &str) -> JsonValue {
    let mut flags = Vec::new();
    let needles: &[(&str, &str)] = &[
        ("curl ", "shell exec — outbound network"),
        ("wget ", "shell exec — outbound network"),
        ("rm -rf /", "destructive shell exec"),
        ("sudo ", "privileged shell exec"),
        ("eval(", "eval — dynamic exec"),
        ("os.system", "python shell exec"),
        ("subprocess.", "python shell exec"),
        ("dd if=", "disk-overwrite shell exec"),
        ("ssh-keygen", "key generation"),
        ("ANTHROPIC_API_KEY", "references API key env"),
        ("OPENAI_API_KEY", "references API key env"),
        ("/etc/passwd", "passwd read"),
        ("base64 -d", "base64 decode (possible obfuscation)"),
    ];
    for (needle, label) in needles {
        if body.contains(needle) {
            flags.push(serde_json::json!({"pattern": needle, "label": label}));
        }
    }
    serde_json::json!({
        "scanner": "ff-skills/static/0.1",
        "flagged": flags.len(),
        "patterns": flags,
    })
}

fn classify_risk(body: &str, tools: &[String]) -> String {
    let mut score: i32 = 0;
    for t in tools {
        if t.contains("Bash") || t.contains("Shell") || t.contains("Exec") {
            score += 2;
        }
        if t.contains("Write") || t.contains("Edit") {
            score += 1;
        }
    }
    for needle in &["sudo ", "rm -rf /", "eval(", "ssh-keygen"] {
        if body.contains(needle) {
            score += 3;
        }
    }
    match score {
        0..=1 => "low".into(),
        2..=4 => "medium".into(),
        _ => "high".into(),
    }
}
