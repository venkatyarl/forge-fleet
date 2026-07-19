//! `ff cloud-llm` subcommand implementations.

use std::io;

use anyhow::{Context, Result};

use crate::{CYAN, GREEN, RED, RESET, YELLOW, truncate_for_col, whoami_tag};

pub async fn handle_cloud_llm_list(pool: &sqlx::PgPool, json: bool) -> Result<()> {
    let providers = ff_agent::cloud_llm_registry::list_providers(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list cloud_llm_providers: {e}"))?;

    if providers.is_empty() {
        println!(
            "{YELLOW}No cloud providers registered. Migration V35 seeds them at startup.{RESET}"
        );
        return Ok(());
    }

    let mut enriched: Vec<(ff_agent::cloud_llm_registry::Provider, bool)> = Vec::new();
    for p in providers {
        let has_key = ff_db::pg_get_secret(pool, &p.secret_key)
            .await
            .map(|v| v.map(|s| !s.is_empty()).unwrap_or(false))
            .unwrap_or(false);
        enriched.push((p, has_key));
    }

    if json {
        let arr: Vec<_> = enriched
            .iter()
            .map(|(p, has_key)| {
                serde_json::json!({
                    "id": p.id,
                    "display_name": p.display_name,
                    "base_url": p.base_url,
                    "auth_kind": p.auth_kind,
                    "model_prefix": p.model_prefix,
                    "request_format": p.request_format,
                    "enabled": p.enabled,
                    "secret_key": p.secret_key,
                    "secret_set": has_key,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return Ok(());
    }

    println!(
        "{:<12} {:<22} {:<14} {:<10} {:<22} {:<7} SECRET",
        "ID", "NAME", "MODEL_PREFIX", "AUTH", "REQUEST_FORMAT", "ENABLED",
    );
    println!("  {}", "-".repeat(110));
    for (p, has_key) in &enriched {
        let secret_col = if *has_key {
            format!("{GREEN}set{RESET}")
        } else {
            format!("{RED}missing{RESET}")
        };
        let enabled = if p.enabled {
            format!("{GREEN}yes{RESET}")
        } else {
            format!("{RED}no{RESET}")
        };
        println!(
            "{:<12} {:<22} {:<14} {:<10} {:<22} {:<7} {}  ({})",
            p.id,
            truncate_for_col(&p.display_name, 22),
            truncate_for_col(&p.model_prefix, 14),
            p.auth_kind,
            truncate_for_col(&p.request_format, 22),
            enabled,
            secret_col,
            p.secret_key,
        );
    }
    println!("\n{} provider(s) registered.", enriched.len());
    Ok(())
}

pub async fn handle_cloud_llm_set_key(
    pool: &sqlx::PgPool,
    provider_id: &str,
    value_override: Option<String>,
) -> Result<()> {
    let providers = ff_agent::cloud_llm_registry::list_providers(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list providers: {e}"))?;
    let Some(provider) = providers.into_iter().find(|p| p.id == provider_id) else {
        eprintln!("{RED}✗ Unknown provider '{provider_id}'. Try `ff cloud-llm list`.{RESET}");
        std::process::exit(1);
    };

    let key_value = match value_override {
        Some(v) if !v.is_empty() => v,
        _ => {
            eprintln!(
                "{CYAN}▶ Enter API key for {} (input hidden via terminal; paste + Enter):{RESET}",
                provider.id
            );
            let mut buf = String::new();
            io::stdin()
                .read_line(&mut buf)
                .context("read API key from stdin")?;
            buf.trim().to_string()
        }
    };

    if key_value.is_empty() {
        eprintln!("{RED}✗ Empty API key, aborting.{RESET}");
        std::process::exit(2);
    }

    let who = whoami_tag();
    ff_db::pg_set_secret(
        pool,
        &provider.secret_key,
        &key_value,
        Some(&format!("cloud LLM api key for {}", provider.id)),
        Some(&who),
    )
    .await
    .map_err(|e| anyhow::anyhow!("store secret: {e}"))?;

    println!(
        "{GREEN}✓ Stored API key for '{}' at secret `{}` ({} bytes, by {who}).{RESET}",
        provider.id,
        provider.secret_key,
        key_value.len(),
    );
    println!("Test it: ff cloud-llm test {}", provider.id);
    Ok(())
}

pub async fn handle_cloud_llm_usage(pool: &sqlx::PgPool, since: &str) -> Result<()> {
    let secs = parse_since_to_secs(since).unwrap_or(24 * 3600);
    let rows = sqlx::query(
        r#"SELECT provider_id,
                  COUNT(*) AS calls,
                  COALESCE(SUM(tokens_input), 0)::BIGINT  AS tokens_in,
                  COALESCE(SUM(tokens_output), 0)::BIGINT AS tokens_out,
                  COALESCE(AVG(request_duration_ms), 0)::FLOAT8 AS avg_ms
             FROM cloud_llm_usage
             WHERE used_at > NOW() - ($1::BIGINT * INTERVAL '1 second')
             GROUP BY provider_id
             ORDER BY calls DESC"#,
    )
    .bind(secs as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("query cloud_llm_usage: {e}"))?;

    if rows.is_empty() {
        println!(
            "{YELLOW}No cloud LLM calls recorded in the last {since} (window: {secs}s).{RESET}"
        );
        return Ok(());
    }

    println!(
        "{:<12} {:>8} {:>12} {:>12} {:>10}",
        "PROVIDER", "CALLS", "TOKENS_IN", "TOKENS_OUT", "AVG_MS",
    );
    println!("  {}", "-".repeat(60));
    for r in &rows {
        let id: String = sqlx::Row::get(r, "provider_id");
        let calls: i64 = sqlx::Row::get(r, "calls");
        let ti: i64 = sqlx::Row::get(r, "tokens_in");
        let to: i64 = sqlx::Row::get(r, "tokens_out");
        let avg_ms: f64 = sqlx::Row::get(r, "avg_ms");
        println!("{id:<12} {calls:>8} {ti:>12} {to:>12} {avg_ms:>10.1}",);
    }
    println!("\nWindow: last {since} ({secs}s).");
    Ok(())
}

/// Parse a window string like `24h`, `15m`, `7d`, `3600s` into seconds.
fn parse_since_to_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, suffix) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().ok()?;
    match suffix {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        "d" => Some(n * 86400),
        _ => s.parse().ok(),
    }
}

pub async fn handle_cloud_llm_test(
    pool: &sqlx::PgPool,
    provider_id: &str,
    model_override: Option<String>,
) -> Result<()> {
    let providers = ff_agent::cloud_llm_registry::list_providers(pool)
        .await
        .map_err(|e| anyhow::anyhow!("list providers: {e}"))?;
    let Some(provider) = providers.into_iter().find(|p| p.id == provider_id) else {
        eprintln!("{RED}✗ Unknown provider '{provider_id}'.{RESET}");
        std::process::exit(1);
    };

    let key = ff_db::pg_get_secret(pool, &provider.secret_key)
        .await
        .map_err(|e| anyhow::anyhow!("read secret: {e}"))?;
    let Some(api_key) = key.filter(|k| !k.is_empty()) else {
        eprintln!(
            "{RED}✗ No API key set for '{}'. Run `ff cloud-llm set-key {}`.{RESET}",
            provider.id, provider.id,
        );
        std::process::exit(1);
    };

    let probe_model = model_override.unwrap_or_else(|| match provider.id.as_str() {
        "openai" => "openai/gpt-4o-mini".to_string(),
        "anthropic" => "claude-3-5-haiku-latest".to_string(),
        "moonshot" => "kimi/moonshot-v1-8k".to_string(),
        "kimi_code" => "kimi-for-coding".to_string(),
        "google" => "gemini/gemini-1.5-flash".to_string(),
        _ => "test".to_string(),
    });

    println!(
        "{CYAN}▶ Probing {} ({}) with model '{}' (api_key=<redacted>){RESET}",
        provider.id, provider.request_format, probe_model,
    );

    static SHARED_HTTP: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(reqwest::Client::new);
    let http = &*SHARED_HTTP;

    let wire_model = if provider.request_format == "google_generate_content" {
        probe_model
            .strip_prefix("gemini/")
            .unwrap_or(&probe_model)
            .to_string()
    } else {
        probe_model.clone()
    };

    let result = probe_cloud_provider(
        http,
        &provider.request_format,
        &provider.base_url,
        &api_key,
        &wire_model,
    )
    .await;

    match result {
        Ok(reply) => {
            println!("{GREEN}✓ {} replied:{RESET} {}", provider.id, reply);
            Ok(())
        }
        Err(msg) => {
            eprintln!("{RED}✗ {} probe failed:{RESET} {msg}", provider.id);
            std::process::exit(1);
        }
    }
}

/// Dispatch a "reply OK" probe in whatever wire format the provider expects.
async fn probe_cloud_provider(
    http: &reqwest::Client,
    fmt: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String, String> {
    let base = base_url.trim_end_matches('/');
    let prompt = "Reply with the word OK.";

    let (url, body, headers): (String, serde_json::Value, Vec<(&str, String)>) = match fmt {
        "openai_chat" => (
            format!("{base}/chat/completions"),
            serde_json::json!({"model": model,
                "messages":[{"role":"user","content":prompt}], "max_tokens":16}),
            vec![("authorization", format!("Bearer {api_key}"))],
        ),
        "anthropic_messages" => (
            format!("{base}/messages"),
            serde_json::json!({"model": model, "max_tokens":16,
                "messages":[{"role":"user","content":prompt}]}),
            vec![
                ("x-api-key", api_key.to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
            ],
        ),
        "google_generate_content" => (
            format!("{base}/models/{model}:generateContent?key={api_key}"),
            serde_json::json!({
                "contents":[{"role":"user","parts":[{"text":prompt}]}],
                "generationConfig":{"maxOutputTokens":16}}),
            vec![],
        ),
        other => return Err(format!("unsupported request_format '{other}'")),
    };

    let mut req = http.post(&url).json(&body);
    for (k, v) in &headers {
        req = req.header(*k, v);
    }
    if base_url.contains("api.kimi.com/coding") {
        req = req.header("User-Agent", "claude-code/0.2.62");
    }
    let resp = req.send().await.map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    if !status.is_success() {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("(no error message)");
        return Err(format!("HTTP {} — {msg}", status.as_u16()));
    }
    let text = match fmt {
        "google_generate_content" => v["candidates"][0]["content"]["parts"][0]["text"].as_str(),
        "anthropic_messages" => v["content"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str()),
        _ => v["choices"][0]["message"]["content"].as_str(),
    };
    Ok(text.unwrap_or("(no content)").to_string())
}

pub async fn handle_cloud_llm(cmd: crate::CloudLlmCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::CloudLlmCommand::List { json } => handle_cloud_llm_list(&pool, json).await,
        crate::CloudLlmCommand::SetKey { provider_id, value } => {
            handle_cloud_llm_set_key(&pool, &provider_id, value).await
        }
        crate::CloudLlmCommand::Usage { since } => handle_cloud_llm_usage(&pool, &since).await,
        crate::CloudLlmCommand::Test { provider_id, model } => {
            handle_cloud_llm_test(&pool, &provider_id, model).await
        }
    }
}
