use crate::{CYAN, GREEN, RED, RESET, YELLOW};
use anyhow::Result;

pub async fn handle_social(cmd: crate::SocialCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    match cmd {
        crate::SocialCommand::Ingest { url, by } => {
            let id = ff_agent::social_ingest::ingest(pool.clone(), url, by).await?;
            println!("{GREEN}✓ ingest queued{RESET}  post_id = {id}");
            // The pipeline runs as a DETACHED task in THIS process. The CLI used
            // to print + exit immediately, which killed that task before it ran —
            // and nothing else drives `queued` posts, so the ingest never
            // completed. Poll until a terminal state (keeping the runtime alive so
            // the task finishes), bounded so a wedged fetch/vision call can't hang.
            let mut last = String::new();
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let row: Option<(String, Option<String>)> = sqlx::query_as(
                    "SELECT status, last_error FROM social_media_posts WHERE id = $1",
                )
                .bind(id)
                .fetch_optional(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("poll ingest status: {e}"))?;
                let Some((status, err)) = row else { break };
                if status != last {
                    println!("  {CYAN}{status}{RESET}");
                    last = status.clone();
                }
                match status.as_str() {
                    "done" => {
                        println!(
                            "{GREEN}✓ analysis done{RESET}  \x1b[2mff social show {id}{RESET}"
                        );
                        return Ok(());
                    }
                    "failed" => {
                        eprintln!(
                            "{RED}✗ ingest failed{RESET}: {}",
                            err.as_deref().unwrap_or("(no error recorded)")
                        );
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
            println!(
                "  \x1b[2mstill running after the wait window — check `ff social show {id}`.{RESET}"
            );
            Ok(())
        }
        crate::SocialCommand::List {
            status,
            platform,
            limit,
        } => {
            let mut sql = String::from(
                "SELECT id, url, platform, status, ingested_by, ingested_at \
                 FROM social_media_posts WHERE 1=1",
            );
            let mut idx = 1;
            if status.is_some() {
                sql.push_str(&format!(" AND status = ${idx}"));
                idx += 1;
            }
            if platform.is_some() {
                sql.push_str(&format!(" AND platform = ${idx}"));
            }
            sql.push_str(" ORDER BY ingested_at DESC LIMIT ");
            sql.push_str(&limit.to_string());

            let mut q = sqlx::query_as::<
                _,
                (
                    uuid::Uuid,
                    String,
                    String,
                    String,
                    Option<String>,
                    chrono::DateTime<chrono::Utc>,
                ),
            >(&sql);
            if let Some(s) = &status {
                q = q.bind(s);
            }
            if let Some(p) = &platform {
                q = q.bind(p);
            }
            let rows = q.fetch_all(&pool).await?;

            println!(
                "{:<38} {:<10} {:<10} {:<16} ingested_at",
                "id", "platform", "status", "by"
            );
            for (id, url, platform, status, by, at) in &rows {
                let url_short: String = url.chars().take(60).collect();
                println!(
                    "{id}  {:<10} {:<10} {:<16} {}",
                    platform,
                    status,
                    by.clone().unwrap_or_default(),
                    at.format("%Y-%m-%d %H:%M")
                );
                println!("  \x1b[2m{url_short}{RESET}");
            }
            println!("\n{} row(s).", rows.len());
            Ok(())
        }
        crate::SocialCommand::Show { id } => {
            let post_id = uuid::Uuid::parse_str(&id)
                .map_err(|e| anyhow::anyhow!("invalid UUID '{id}': {e}"))?;
            let row: Option<(
                uuid::Uuid,
                String,
                String,
                Option<String>,
                Option<String>,
                serde_json::Value,
                Option<String>,
                Option<serde_json::Value>,
                String,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT id, url, platform, author, caption, media_items, \
                        extracted_text, analysis, status, ingested_by, \
                        ingested_at, analyzed_at, last_error \
                 FROM social_media_posts WHERE id = $1",
            )
            .bind(post_id)
            .fetch_optional(&pool)
            .await?;
            let Some((
                id,
                url,
                platform,
                author,
                caption,
                media_items,
                extracted_text,
                analysis,
                status,
                ingested_by,
                ingested_at,
                analyzed_at,
                last_error,
            )) = row
            else {
                println!("{RED}✗ no social_media_posts row with id = {id}{RESET}");
                return Ok(());
            };

            println!("{CYAN}post{RESET}    {id}");
            println!("url      {url}");
            println!("platform {platform}");
            println!("status   {status}");
            println!("by       {}", ingested_by.unwrap_or_default());
            println!("ingested {}", ingested_at.format("%Y-%m-%d %H:%M:%S"));
            if let Some(a) = analyzed_at {
                println!("analyzed {}", a.format("%Y-%m-%d %H:%M:%S"));
            }
            if let Some(a) = author {
                println!("author   {a}");
            }
            if let Some(c) = caption {
                let trunc = if c.chars().count() > 400 {
                    format!("{}…", c.chars().take(400).collect::<String>())
                } else {
                    c
                };
                println!("caption  {trunc}");
            }
            let media_arr = media_items.as_array().cloned().unwrap_or_default();
            println!("media    {} item(s)", media_arr.len());
            for m in &media_arr {
                let kind = m.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                let path = m.get("local_path").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  • [{kind}] {path}");
            }
            if let Some(t) = extracted_text
                && !t.trim().is_empty()
            {
                println!("\n{CYAN}extracted_text{RESET}\n{t}");
            }
            if let Some(a) = analysis {
                let pretty = serde_json::to_string_pretty(&a).unwrap_or_default();
                println!("\n{CYAN}analysis{RESET}\n{pretty}");
            }
            if let Some(e) = last_error {
                println!("\n{RED}last_error{RESET} {e}");
            }
            Ok(())
        }
        crate::SocialCommand::Check => {
            let host = ff_agent::fleet_info::resolve_this_worker_name().await;
            println!("{CYAN}▶ social-ingest deps on {host}{RESET}");
            let deps = ff_agent::social_ingest::fetcher::preflight().await;
            let mut all_ok = true;
            for d in &deps {
                if d.ok {
                    println!("  {GREEN}✓{RESET} {:<8} {}", d.name, d.note);
                } else {
                    all_ok = false;
                    println!("  {RED}✗{RESET} {:<8} {}", d.name, d.note);
                }
            }
            // yt-dlp gates everything; ffmpeg only video frames.
            let ytdlp_ok = deps
                .iter()
                .find(|d| d.name == "yt-dlp")
                .is_some_and(|d| d.ok);
            if all_ok {
                println!("{GREEN}✓ ready — this host can ingest images and video.{RESET}");
            } else if ytdlp_ok {
                println!(
                    "{YELLOW}⚠ images-only — yt-dlp present but ffmpeg missing; video posts \
                     will fail at frame extraction.{RESET}"
                );
            } else {
                eprintln!("{RED}✗ not ready — yt-dlp is required for any ingest.{RESET}");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
