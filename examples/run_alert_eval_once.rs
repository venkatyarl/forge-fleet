//! Manual one-shot trigger for the alert evaluator.
//!
//! Usage:
//!   cargo run --release --example run_alert_eval_once
//!
//! This sidesteps the 60s daemon loop (and its leader gate) and runs a
//! single `AlertEvaluator::evaluate_once()` pass against the live DB +
//! Redis so you can exercise dispatch end-to-end. Useful for:
//!   - verifying the `log` channel writes a proper alert_events row
//!   - checking that `telegram`/`webhook` fail gracefully when secrets
//!     are missing (status string, no panic)
//!   - smoke-testing new policies before enabling them fleet-wide.

use ff_agent::alert_evaluator::AlertEvaluator;
use ff_pulse::reader::PulseReader;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,alerts=info")),
        )
        .init();

    let pg_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://forgefleet:forgefleet@localhost:55432/forgefleet".to_string()
    });
    let redis_url = std::env::var("FORGEFLEET_REDIS_URL")
        .unwrap_or_else(|_| "redis://localhost:6380".to_string());

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&pg_url)
        .await?;
    let pulse = PulseReader::new(&redis_url)?;
    let my_name = ff_agent::fleet_info::resolve_this_node_name().await;

    println!("▶ run_alert_eval_once");
    println!("  pg:    {pg_url}");
    println!("  redis: {redis_url}");
    println!("  node:  {my_name}");

    let evaluator = AlertEvaluator::new(pool, pulse, my_name);
    let report = evaluator.evaluate_once().await?;

    println!("\neval report:");
    println!("  policies_evaluated = {}", report.policies_evaluated);
    println!("  events_fired       = {}", report.events_fired);
    println!("  events_resolved    = {}", report.events_resolved);

    Ok(())
}
