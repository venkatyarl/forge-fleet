use crate::whoami_tag;
use anyhow::{Result, bail};
use sqlx::PgPool;

const FIELDS: [&str; 5] = [
    "base_url",
    "project_key",
    "auth_email",
    "token_secret_key",
    "instructions",
];

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
        .ok_or_else(|| anyhow::anyhow!("No Jira configuration value set for key: {key}"))
}

async fn set_field(pool: &PgPool, alias: &str, field: &str, value: &str) -> Result<()> {
    let key = jira_key(alias, field);
    let who = whoami_tag();
    ff_db::pg_set_secret(pool, &key, value, None, Some(&who)).await?;
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
                let key = jira_key(&alias, field);
                let value = ff_db::pg_get_secret(&pool, &key).await?.unwrap_or_default();
                println!("{field}: {value}");
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
            set_field(&pool, &alias, "base_url", &base_url).await?;
            set_field(&pool, &alias, "project_key", &project_key).await?;
            set_field(&pool, &alias, "auth_email", &auth_email).await?;
            set_field(&pool, &alias, "token_secret_key", &token_secret_key).await?;
            if let Some(instructions) = instructions {
                set_field(&pool, &alias, "instructions", &instructions).await?;
            }
            println!("Jira site '{alias}' stored");
        }
        crate::JiraCommand::Instructions { alias, text } => {
            validate_alias(&alias)?;
            match text {
                Some(text) => {
                    set_field(&pool, &alias, "instructions", &text).await?;
                    println!("Jira instructions for '{alias}' stored");
                }
                None => println!(
                    "{}",
                    get_required(&pool, &jira_key(&alias, "instructions")).await?
                ),
            }
        }
        crate::JiraCommand::Token { alias } => {
            validate_alias(&alias)?;
            let token_key = get_required(&pool, &jira_key(&alias, "token_secret_key")).await?;
            println!("{}", get_required(&pool, &token_key).await?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_well_formed_base_url_keys() {
        assert_eq!(alias_from_base_url_key("jira.prod.base_url"), Some("prod"));
        assert_eq!(alias_from_base_url_key("jira.prod.project_key"), None);
        assert_eq!(alias_from_base_url_key("jira.team.prod.base_url"), None);
        assert_eq!(alias_from_base_url_key("jira..base_url"), None);
    }

    #[test]
    fn rejects_aliases_that_break_the_namespace() {
        assert!(validate_alias("").is_err());
        assert!(validate_alias("team.prod").is_err());
        assert!(validate_alias("prod").is_ok());
    }
}
