use crate::whoami_tag;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool};
use std::{cmp::Ordering, time::Duration};
use uuid::Uuid;

const FIELDS: [&str; 5] = [
    "base_url",
    "project_key",
    "auth_email",
    "token_secret_key",
    "instructions",
];

#[derive(Clone, FromRow)]
struct MonitorConfig {
    name: String,
    project_key: String,
    owner_account_id: String,
    jira_secret_ref: String,
    poll_interval_s: i32,
    retag_after_s: i32,
    queue_jql: String,
    ruleset_id: String,
    label_policy_json: Value,
    transition_policy_json: Value,
    repo_map_json: Value,
    cwd_path_globs: Vec<String>,
    version: i32,
}

#[derive(Clone)]
struct JiraAuth {
    base_url: String,
    email: String,
    token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JiraSearch {
    #[serde(default)]
    issues: Vec<JiraIssue>,
}

#[derive(Debug, Clone, Deserialize)]
struct JiraIssue {
    id: String,
    key: String,
    fields: JiraFields,
}

#[derive(Debug, Clone, Deserialize)]
struct JiraFields {
    summary: String,
    created: DateTime<Utc>,
    status: JiraNamed,
    assignee: Option<JiraUser>,
    reporter: Option<JiraUser>,
    priority: Option<JiraPriority>,
    issuetype: Option<JiraNamed>,
    #[serde(default)]
    labels: Vec<String>,
    comment: Option<JiraComments>,
}

#[derive(Debug, Clone, Deserialize)]
struct JiraNamed {
    name: String,
}
#[derive(Debug, Clone, Deserialize)]
struct JiraPriority {
    name: String,
    id: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
struct JiraUser {
    #[serde(rename = "accountId")]
    account_id: String,
}
#[derive(Debug, Clone, Deserialize)]
struct JiraComments {
    #[serde(default)]
    comments: Vec<JiraComment>,
}
#[derive(Debug, Clone, Deserialize)]
struct JiraComment {
    id: String,
    created: DateTime<Utc>,
    author: JiraUser,
}

fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() || alias.contains('.') {
        bail!("Jira alias must be non-empty and cannot contain '.'");
    }
    Ok(())
}

fn jira_key(alias: &str, field: &str) -> String {
    format!("jira.{alias}.{field}")
}

fn alias_from_base_url_key(key: &str) -> Option<&str> {
    key.strip_prefix("jira.")
        .and_then(|rest| rest.strip_suffix(".base_url"))
        .filter(|alias| !alias.is_empty() && !alias.contains('.'))
}

async fn get_required(pool: &PgPool, key: &str) -> Result<String> {
    ff_db::pg_get_secret(pool, key)
        .await?
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("No Jira configuration value set for key: {key}"))
}

async fn set_field(pool: &PgPool, alias: &str, field: &str, value: &str) -> Result<()> {
    ff_db::pg_set_secret(
        pool,
        &jira_key(alias, field),
        value,
        None,
        Some(&whoami_tag()),
    )
    .await?;
    Ok(())
}

async fn load_config(pool: &PgPool, requested: Option<&str>) -> Result<MonitorConfig> {
    if let Some(name) = requested {
        return sqlx::query_as("SELECT * FROM jira_configs WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown Jira monitor config '{name}'"));
    }
    let cwd = std::env::current_dir()?
        .to_string_lossy()
        .replace('\\', "/");
    let configs: Vec<MonitorConfig> = sqlx::query_as("SELECT * FROM jira_configs ORDER BY name")
        .fetch_all(pool)
        .await?;
    let mut matches = configs
        .into_iter()
        .filter(|c| c.cwd_path_globs.iter().any(|g| path_glob_matches(g, &cwd)));
    let found = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("no Jira config maps CWD {cwd}; pass --config <name>"))?;
    if matches.next().is_some() {
        bail!("multiple Jira configs map CWD {cwd}; pass --config <name>");
    }
    Ok(found)
}

fn path_glob_matches(glob: &str, path: &str) -> bool {
    let needle = glob.trim_matches('*').trim_end_matches('/');
    !needle.is_empty() && path.contains(needle)
}

async fn load_auth(pool: &PgPool, c: &MonitorConfig) -> Result<JiraAuth> {
    Ok(JiraAuth {
        base_url: get_required(pool, &jira_key(&c.name, "base_url"))
            .await?
            .trim_end_matches('/')
            .into(),
        email: get_required(pool, &jira_key(&c.name, "auth_email")).await?,
        token: get_required(pool, &c.jira_secret_ref).await?,
    })
}

async fn fetch_queue(
    client: &reqwest::Client,
    auth: &JiraAuth,
    c: &MonitorConfig,
) -> Result<Vec<JiraIssue>> {
    let response = client
        .get(format!("{}/rest/api/3/search/jql", auth.base_url))
        .basic_auth(&auth.email, Some(&auth.token))
        .query(&[
            ("jql", c.queue_jql.as_str()),
            (
                "fields",
                "summary,created,status,assignee,reporter,priority,issuetype,labels,comment",
            ),
            ("maxResults", "100"),
        ])
        .send()
        .await
        .context("query Jira queue")?
        .error_for_status()
        .context("Jira queue response")?;
    let mut issues = response
        .json::<JiraSearch>()
        .await
        .context("decode Jira queue")?
        .issues;
    issues.sort_by(queue_cmp);
    Ok(issues)
}

fn queue_cmp(a: &JiraIssue, b: &JiraIssue) -> Ordering {
    queue_rank(a)
        .cmp(&queue_rank(b))
        .then_with(|| a.fields.created.cmp(&b.fields.created))
        .then_with(|| a.key.cmp(&b.key))
}

fn queue_rank(i: &JiraIssue) -> (u8, u32) {
    let s = i.fields.summary.to_ascii_lowercase();
    let kind = i
        .fields
        .issuetype
        .as_ref()
        .map(|x| x.name.to_ascii_lowercase())
        .unwrap_or_default();
    let bucket = if s.starts_with("blocker:") {
        0
    } else if s.starts_with("priority:") {
        1
    } else if kind == "bug" {
        5
    } else {
        6
    };
    let priority = i
        .fields
        .priority
        .as_ref()
        .and_then(|p| p.id.as_deref())
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX);
    (bucket, priority)
}

async fn acquire_monitor_lease(pool: &PgPool, c: &MonitorConfig, session: &str) -> Result<Uuid> {
    let token = Uuid::new_v4();
    let lease_s = i64::from(c.poll_interval_s.max(30)) * 2;
    let acquired: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO jira_monitor_leases(config_id, session_id, lease_token, heartbeat_at, lease_until)
         VALUES ($1,$2,$3,NOW(),NOW()+make_interval(secs => $4))
         ON CONFLICT(config_id) DO UPDATE SET session_id=EXCLUDED.session_id,
           lease_token=EXCLUDED.lease_token, heartbeat_at=NOW(), lease_until=EXCLUDED.lease_until
         WHERE jira_monitor_leases.lease_until < NOW() OR jira_monitor_leases.session_id=$2
         RETURNING lease_token")
        .bind(&c.name).bind(session).bind(token).bind(lease_s as f64)
        .fetch_optional(pool).await?;
    acquired
        .ok_or_else(|| anyhow::anyhow!("monitor lease for '{}' is held by another session", c.name))
}

async fn renew_monitor_lease(
    pool: &PgPool,
    c: &MonitorConfig,
    session: &str,
    token: Uuid,
) -> Result<()> {
    let lease_s = i64::from(c.poll_interval_s.max(30)) * 2;
    let updated = sqlx::query(
        "UPDATE jira_monitor_leases SET heartbeat_at=NOW(), lease_until=NOW()+make_interval(secs => $4)
         WHERE config_id=$1 AND session_id=$2 AND lease_token=$3 AND lease_until >= NOW()")
        .bind(&c.name).bind(session).bind(token).bind(lease_s as f64).execute(pool).await?.rows_affected();
    if updated != 1 {
        bail!("lost monitor lease for '{}'", c.name);
    }
    Ok(())
}

async fn reconcile_once(
    pool: &PgPool,
    client: &reqwest::Client,
    auth: &JiraAuth,
    c: &MonitorConfig,
    session: &str,
    token: Uuid,
    dry_run: bool,
) -> Result<Vec<JiraIssue>> {
    renew_monitor_lease(pool, c, session, token).await?;
    let issues = fetch_queue(client, auth, c).await?;
    let mut tx = pool.begin().await?;
    for issue in &issues {
        let latest = issue
            .fields
            .comment
            .as_ref()
            .and_then(|x| x.comments.iter().max_by_key(|c| c.created));
        let previous: Option<(Option<String>, Option<DateTime<Utc>>, Option<String>, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT last_seen_comment_id,last_seen_comment_created_at,last_seen_status,awaiting_party,awaiting_since,last_retag_at FROM jira_watch_state WHERE config_id=$1 AND issue_id=$2 FOR UPDATE")
                .bind(&c.name).bind(&issue.id).fetch_optional(&mut *tx).await?;
        let newer_human = match (latest, previous.as_ref()) {
            (Some(comment), Some((_, seen, _, _, _, _))) => {
                comment.author.account_id != c.owner_account_id
                    && seen.map(|v| comment.created > v).unwrap_or(true)
            }
            _ => false,
        };
        let awaiting_party = if newer_human {
            None
        } else {
            previous.as_ref().and_then(|x| x.3.clone())
        };
        let awaiting_since = if newer_human {
            None
        } else {
            previous.as_ref().and_then(|x| x.4)
        };
        let prior_status = previous.as_ref().and_then(|x| x.2.as_deref());
        let event_kind = if previous.is_none() {
            "new-assigned"
        } else if prior_status.is_some_and(|s| s != issue.fields.status.name) {
            if prior_status.is_some_and(|s| s.eq_ignore_ascii_case("done")) {
                "reopen"
            } else {
                "status-change"
            }
        } else if newer_human {
            "reply"
        } else if previous.as_ref().and_then(|x| x.0.as_deref()) != latest.map(|x| x.id.as_str()) {
            "comment"
        } else {
            "observed"
        };
        let cursor = latest
            .map(|x| format!("{}:{}", x.id, x.created.timestamp_millis()))
            .unwrap_or_else(|| "none".into());
        let event_key = format!("{}:{}:{}:{}", c.name, issue.id, event_kind, cursor);
        sqlx::query("INSERT INTO jira_action_log(event_key,config_id,issue_id,kind,payload_json) VALUES($1,$2,$3,$4,$5) ON CONFLICT(event_key) DO NOTHING")
            .bind(event_key).bind(&c.name).bind(&issue.id).bind(event_kind)
            .bind(json!({"key":issue.key,"summary":issue.fields.summary,"dry_run":dry_run}))
            .execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO jira_watch_state(config_id,issue_id,last_seen_comment_id,last_seen_comment_created_at,last_seen_status,last_seen_assignee_id,awaiting_party,awaiting_since,state_json)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT(config_id,issue_id) DO UPDATE SET last_seen_comment_id=EXCLUDED.last_seen_comment_id,
               last_seen_comment_created_at=EXCLUDED.last_seen_comment_created_at,last_seen_status=EXCLUDED.last_seen_status,
               last_seen_assignee_id=EXCLUDED.last_seen_assignee_id,awaiting_party=EXCLUDED.awaiting_party,
               awaiting_since=EXCLUDED.awaiting_since,state_json=EXCLUDED.state_json")
            .bind(&c.name).bind(&issue.id).bind(latest.map(|x| &x.id)).bind(latest.map(|x| x.created))
            .bind(&issue.fields.status.name).bind(issue.fields.assignee.as_ref().map(|x| &x.account_id))
            .bind(awaiting_party).bind(awaiting_since)
            .bind(json!({"key":issue.key,"labels":issue.fields.labels,"reporter":issue.fields.reporter.as_ref().map(|r| &r.account_id)}))
            .execute(&mut *tx).await?;
        if let Some(since) = awaiting_since {
            let last_retag = previous.as_ref().and_then(|x| x.5).unwrap_or(since);
            let due = last_retag + chrono::Duration::seconds(i64::from(c.retag_after_s));
            if due <= Utc::now() {
                let retag_key = format!("{}:{}:retag-due:{}", c.name, issue.id, due.timestamp());
                sqlx::query("INSERT INTO jira_action_log(event_key,config_id,issue_id,kind,payload_json) VALUES($1,$2,$3,'retag-due',$4) ON CONFLICT(event_key) DO NOTHING")
                    .bind(retag_key).bind(&c.name).bind(&issue.id)
                    .bind(json!({"key":issue.key,"awaiting_since":since,"outcome":if dry_run{"planned"}else{"emitted"}}))
                    .execute(&mut *tx).await?;
                sqlx::query("UPDATE jira_watch_state SET last_retag_at=NOW(),next_action_at=NOW()+make_interval(secs => $3) WHERE config_id=$1 AND issue_id=$2")
                    .bind(&c.name).bind(&issue.id).bind(f64::from(c.retag_after_s)).execute(&mut *tx).await?;
            } else {
                sqlx::query("UPDATE jira_watch_state SET next_action_at=$3 WHERE config_id=$1 AND issue_id=$2")
                    .bind(&c.name).bind(&issue.id).bind(due).execute(&mut *tx).await?;
            }
        }
    }
    tx.commit().await?;
    for issue in &issues {
        sqlx::query(
            "INSERT INTO jira_issue_leases(config_id,issue_id,session_id,lease_token,heartbeat_at,lease_until)
             VALUES($1,$2,$3,$4,NOW(),NOW()+make_interval(secs => $5))
             ON CONFLICT(config_id,issue_id) DO UPDATE SET session_id=EXCLUDED.session_id,
               lease_token=EXCLUDED.lease_token,heartbeat_at=NOW(),lease_until=EXCLUDED.lease_until
             WHERE jira_issue_leases.lease_until<NOW() OR jira_issue_leases.session_id=$3",
        ).bind(&c.name).bind(&issue.id).bind(session).bind(Uuid::new_v4())
         .bind(f64::from(c.poll_interval_s.max(30))*2.0).execute(pool).await?;
    }
    Ok(issues)
}

async fn validate_config(pool: &PgPool, name: &str) -> Result<()> {
    let c = load_config(pool, Some(name)).await?;
    if c.queue_jql.trim().is_empty() {
        bail!("queue_jql is empty");
    }
    if !c
        .queue_jql
        .to_ascii_uppercase()
        .contains(&c.project_key.to_ascii_uppercase())
    {
        bail!("queue_jql does not constrain project '{}'", c.project_key);
    }
    let normalized = c.queue_jql.to_ascii_lowercase().replace(' ', "");
    if !normalized.contains("assignee=currentuser()")
        || !normalized.contains("statuscategory!=done")
    {
        bail!("queue_jql must retain assignee=currentUser() AND statusCategory != Done");
    }
    let _: (String, bool) =
        sqlx::query_as("SELECT content_hash,active FROM jira_rulesets WHERE id=$1 AND active")
            .bind(&c.ruleset_id)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("ruleset '{}' is missing or inactive", c.ruleset_id))?;
    if !c.label_policy_json.is_object()
        || !c.transition_policy_json.is_object()
        || !c.repo_map_json.is_object()
    {
        bail!("label, transition, and repo-map policies must be JSON objects");
    }
    if c.cwd_path_globs.is_empty() {
        bail!("cwd_path_globs is empty");
    }
    let auth = load_auth(pool, &c).await?;
    reqwest::Client::new()
        .get(format!("{}/rest/api/3/myself", auth.base_url))
        .basic_auth(auth.email, Some(auth.token))
        .send()
        .await?
        .error_for_status()
        .context("Jira credential validation")?;
    println!(
        "{}: valid (schema version {}, poll {}s, retag {}s)",
        c.name, c.version, c.poll_interval_s, c.retag_after_s
    );
    Ok(())
}

async fn show_queue(pool: &PgPool, requested: Option<&str>) -> Result<()> {
    let c = load_config(pool, requested).await?;
    let auth = load_auth(pool, &c).await?;
    let issues = fetch_queue(&reqwest::Client::new(), &auth, &c).await?;
    println!("RANK KEY       PRIORITY     STATUS               LEASE");
    for (idx, issue) in issues.iter().enumerate() {
        let holder: Option<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT session_id,lease_until FROM jira_issue_leases WHERE config_id=$1 AND issue_id=$2 AND lease_until>NOW()")
            .bind(&c.name).bind(&issue.id).fetch_optional(pool).await?;
        let lease = holder
            .map(|(s, u)| format!("{s} until {u}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<4} {:<9} {:<12} {:<20} {}",
            idx + 1,
            issue.key,
            issue
                .fields
                .priority
                .as_ref()
                .map(|x| x.name.as_str())
                .unwrap_or("-"),
            issue.fields.status.name,
            lease
        );
    }
    Ok(())
}

async fn run_monitor(
    pool: &PgPool,
    requested: Option<&str>,
    daemon: bool,
    once: bool,
    dry_run: bool,
) -> Result<()> {
    if !daemon && !once && !dry_run {
        bail!("choose --daemon, --once, or --dry-run");
    }
    let c = load_config(pool, requested).await?;
    validate_config_shape(pool, &c).await?;
    let auth = load_auth(pool, &c).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    if dry_run {
        let issues = fetch_queue(&client, &auth, &c).await?;
        println!(
            "{}: would reconcile {} assigned issue(s) (dry-run)",
            c.name,
            issues.len()
        );
        return Ok(());
    }
    let session = format!("{}-{}", whoami_tag(), Uuid::new_v4());
    let token = acquire_monitor_lease(pool, &c, &session).await?;
    loop {
        let issues = reconcile_once(pool, &client, &auth, &c, &session, token, dry_run).await?;
        println!(
            "{}: reconciled {} assigned issue(s){}",
            c.name,
            issues.len(),
            if dry_run { " (dry-run)" } else { "" }
        );
        if !daemon {
            break;
        }
        tokio::time::sleep(Duration::from_secs(c.poll_interval_s as u64)).await;
    }
    Ok(())
}

async fn validate_config_shape(pool: &PgPool, c: &MonitorConfig) -> Result<()> {
    let active: Option<bool> = sqlx::query_scalar("SELECT active FROM jira_rulesets WHERE id=$1")
        .bind(&c.ruleset_id)
        .fetch_optional(pool)
        .await?;
    if active != Some(true) {
        bail!("ruleset '{}' is missing or inactive", c.ruleset_id);
    }
    Ok(())
}

pub async fn handle_jira(cmd: crate::JiraCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;
    match cmd {
        crate::JiraCommand::List => {
            let mut aliases: Vec<String> = ff_db::pg_list_secrets(&pool)
                .await?
                .into_iter()
                .filter_map(|(key, _, _, _)| alias_from_base_url_key(&key).map(str::to_owned))
                .collect();
            aliases.sort();
            aliases.dedup();
            if aliases.is_empty() {
                println!("(no Jira sites configured)");
            } else {
                for alias in aliases {
                    println!("{alias}");
                }
            }
        }
        crate::JiraCommand::Get { alias } => {
            validate_alias(&alias)?;
            for field in FIELDS {
                println!(
                    "{field}: {}",
                    ff_db::pg_get_secret(&pool, &jira_key(&alias, field))
                        .await?
                        .unwrap_or_default()
                );
            }
        }
        crate::JiraCommand::Set {
            alias,
            base_url,
            project_key,
            auth_email,
            token_secret_key,
            instructions,
        } => {
            validate_alias(&alias)?;
            for (field, value) in [
                ("base_url", base_url),
                ("project_key", project_key),
                ("auth_email", auth_email),
                ("token_secret_key", token_secret_key),
            ] {
                set_field(&pool, &alias, field, &value).await?;
            }
            if let Some(v) = instructions {
                set_field(&pool, &alias, "instructions", &v).await?;
            }
            println!("Jira site '{alias}' stored");
        }
        crate::JiraCommand::Instructions { alias, text } => {
            validate_alias(&alias)?;
            if let Some(v) = text {
                set_field(&pool, &alias, "instructions", &v).await?;
                println!("Jira instructions for '{alias}' stored");
            } else {
                println!(
                    "{}",
                    get_required(&pool, &jira_key(&alias, "instructions")).await?
                );
            }
        }
        crate::JiraCommand::Token { alias } => {
            validate_alias(&alias)?;
            let key = get_required(&pool, &jira_key(&alias, "token_secret_key")).await?;
            println!("{}", get_required(&pool, &key).await?);
        }
        crate::JiraCommand::Monitor {
            command: Some(crate::JiraMonitorCommand::Status),
            config,
            ..
        } => {
            let c = load_config(&pool, config.as_deref()).await?;
            let row: Option<(String, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as("SELECT session_id,heartbeat_at,lease_until FROM jira_monitor_leases WHERE config_id=$1")
                .bind(&c.name).fetch_optional(&pool).await?;
            match row {
                Some((s, h, u)) => println!(
                    "{}: holder={s} heartbeat={h} lease_until={u} active={}",
                    c.name,
                    u > Utc::now()
                ),
                None => println!("{}: stopped", c.name),
            }
        }
        crate::JiraCommand::Monitor {
            command: Some(crate::JiraMonitorCommand::Stop),
            config,
            ..
        } => {
            let c = load_config(&pool, config.as_deref()).await?;
            sqlx::query("DELETE FROM jira_monitor_leases WHERE config_id=$1")
                .bind(&c.name)
                .execute(&pool)
                .await?;
            println!("{}: monitor lease revoked", c.name);
        }
        crate::JiraCommand::Monitor {
            command: None,
            config,
            daemon,
            once,
            dry_run,
        } => run_monitor(&pool, config.as_deref(), daemon, once, dry_run).await?,
        crate::JiraCommand::Queue { config } => show_queue(&pool, config.as_deref()).await?,
        crate::JiraCommand::Reconcile { config, dry_run } => {
            run_monitor(&pool, config.as_deref(), false, true, dry_run).await?
        }
        crate::JiraCommand::Config {
            command: crate::JiraConfigCommand::Validate { name },
        } => validate_config(&pool, &name).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn issue(summary: &str, kind: &str, created: &str) -> JiraIssue {
        JiraIssue {
            id: "1".into(),
            key: "HFPROD-1".into(),
            fields: JiraFields {
                summary: summary.into(),
                created: created.parse().unwrap(),
                status: JiraNamed {
                    name: "To Do".into(),
                },
                assignee: None,
                reporter: None,
                priority: None,
                issuetype: Some(JiraNamed { name: kind.into() }),
                labels: vec![],
                comment: None,
            },
        }
    }
    #[test]
    fn recognizes_only_well_formed_base_url_keys() {
        assert_eq!(alias_from_base_url_key("jira.prod.base_url"), Some("prod"));
        assert_eq!(alias_from_base_url_key("jira.team.prod.base_url"), None);
    }
    #[test]
    fn cwd_glob_mapping_handles_double_star_contract() {
        assert!(path_glob_matches(
            "**/projects/HireFlow360/**",
            "/home/a/projects/HireFlow360/api"
        ));
        assert!(!path_glob_matches(
            "**/projects/HireFlow360/**",
            "/home/a/projects/elsewhere"
        ));
    }
    #[test]
    fn queue_starts_with_blocker_then_priority_then_oldest_bug() {
        let mut v = vec![
            issue("normal", "Task", "2025-01-01T00:00:00Z"),
            issue("Priority: now", "Task", "2025-01-02T00:00:00Z"),
            issue("Blocker: down", "Bug", "2025-01-03T00:00:00Z"),
            issue("old bug", "Bug", "2024-01-01T00:00:00Z"),
        ];
        v.sort_by(queue_cmp);
        assert_eq!(
            v.iter()
                .map(|x| x.fields.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["Blocker: down", "Priority: now", "old bug", "normal"]
        );
    }
}
