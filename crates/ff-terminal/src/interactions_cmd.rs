use anyhow::Result;

/// `ff interactions stats` — health snapshot of the `ff_interactions` training
/// corpus (the dogfooding req+resp+tokens log). Read-only.
pub async fn handle_interactions(cmd: crate::InteractionsCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    match cmd {
        crate::InteractionsCommand::Stats { window_hours, json } => {
            let stats = ff_db::pg_interaction_stats(&pool, window_hours).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                print!("{}", render_interaction_stats(&stats));
            }
        }
    }
    Ok(())
}

/// Render an [`ff_db::InteractionStats`] as a human report (pure; unit-tested).
fn render_interaction_stats(s: &ff_db::InteractionStats) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "ff_interactions corpus — {} rows total ({} in last {}h)\n",
        s.total, s.recent, s.window_hours
    ));
    out.push_str(&format!(
        "  tokens logged: {} in / {} out (lifetime)\n",
        s.total_tokens_in, s.total_tokens_out
    ));
    // Logging-gap meter: rows with no token data in the window.
    let gap = if s.recent > 0 {
        100 * s.recent_zero_token / s.recent
    } else {
        0
    };
    out.push_str(&format!(
        "  token-logging gap (last {}h): {}/{} rows missing tokens ({gap}%)\n",
        s.window_hours, s.recent_zero_token, s.recent
    ));

    out.push_str("\n  by channel (count · success · tok-in/out):\n");
    if s.by_channel.is_empty() {
        out.push_str("    (none)\n");
    }
    for c in &s.by_channel {
        out.push_str(&format!(
            "    {:<20} {:>5} · {:>3} ok · {}/{}\n",
            c.channel, c.count, c.success, c.tokens_in, c.tokens_out
        ));
    }

    out.push_str(&format!("\n  errors in last {}h:\n", s.window_hours));
    if s.recent_errors.is_empty() {
        out.push_str("    (none) ✓\n");
    }
    for e in &s.recent_errors {
        out.push_str(&format!("    {:>4}  {}\n", e.count, e.label));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::render_interaction_stats;

    fn chan(
        channel: &str,
        count: i64,
        success: i64,
        ti: i64,
        to: i64,
    ) -> ff_db::InteractionChannelStat {
        ff_db::InteractionChannelStat {
            channel: channel.into(),
            count,
            success,
            tokens_in: ti,
            tokens_out: to,
        }
    }

    #[test]
    fn renders_sections_totals_and_gap_pct() {
        let s = ff_db::InteractionStats {
            window_hours: 24,
            total: 100,
            recent: 8,
            total_tokens_in: 5000,
            total_tokens_out: 9000,
            recent_zero_token: 2,
            by_channel: vec![
                chan("research_subtask", 40, 38, 1000, 2000),
                chan("council_member", 12, 12, 0, 0),
            ],
            recent_errors: vec![ff_db::DeferredCount {
                label: "timeout".into(),
                count: 3,
            }],
        };
        let out = render_interaction_stats(&s);
        assert!(out.contains("100 rows total (8 in last 24h)"));
        assert!(out.contains("5000 in / 9000 out"));
        assert!(out.contains("2/8 rows missing tokens (25%)")); // 2/8 = 25%
        assert!(out.contains("research_subtask"));
        assert!(out.contains("3  timeout"));
    }

    #[test]
    fn handles_empty_corpus_without_div_by_zero() {
        let s = ff_db::InteractionStats::default();
        let out = render_interaction_stats(&s);
        assert!(out.contains("0 rows total"));
        assert!(out.contains("(0%)")); // recent==0 → guarded, no panic
        assert!(out.contains("(none) ✓"));
    }
}
