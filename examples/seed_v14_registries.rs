use sqlx::postgres::PgPoolOptions;
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://forgefleet:forgefleet@localhost:55432/forgefleet".to_string());
    let pool = PgPoolOptions::new().max_connections(4).connect(&url).await?;

    println!("▶ Seeding software_registry from config/software.toml ...");
    let sw = ff_agent::software_registry::seed_from_toml(
        &pool,
        Path::new("config/software.toml"),
    ).await?;
    println!("  inserted={} updated={} unchanged={} total={}",
        sw.inserted, sw.updated, sw.unchanged, sw.total);

    println!("▶ Seeding V14 model_catalog from config/model_catalog.toml ...");
    let mc = ff_agent::seed_model_catalog_from_toml(
        &pool,
        Path::new("config/model_catalog.toml"),
    ).await?;
    println!("  inserted={} updated={} unchanged={} skipped={} total={}",
        mc.inserted, mc.updated, mc.unchanged, mc.skipped_invalid, mc.total);

    println!("✓ Done.");
    Ok(())
}
