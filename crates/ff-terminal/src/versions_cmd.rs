use anyhow::Result;
use crate::truncate_for_col;

pub async fn handle_versions(node_filter: Option<String>) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(|e| anyhow::anyhow!("connect Postgres: {e}"))?;
    ff_db::run_postgres_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("run_postgres_migrations: {e}"))?;

    let nodes = ff_db::pg_list_nodes(&pool).await?;
    let filtered: Vec<&ff_db::FleetNodeRow> = nodes
        .iter()
        .filter(|n| node_filter.as_deref().map(|f| n.name == f).unwrap_or(true))
        .collect();

    let mut all_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for n in &filtered {
        if let Some(obj) = n.tooling.as_object() {
            for k in obj.keys() {
                all_keys.insert(k.clone());
            }
        }
    }
    if all_keys.is_empty() {
        println!(
            "(no tool-version data yet — run `ff daemon` for 6h or manually trigger version_check)"
        );
        return Ok(());
    }

    print!("{:<14}", "TOOL");
    for n in &filtered {
        print!(" {:<14}", truncate_for_col(&n.name, 14));
    }
    println!();
    for k in &all_keys {
        print!("{:<14}", truncate_for_col(k, 14));
        for n in &filtered {
            let cell = n.tooling.get(k);
            let (cur, lat) = match cell {
                Some(obj) => (
                    obj.get("current").and_then(|v| v.as_str()).unwrap_or("-"),
                    obj.get("latest").and_then(|v| v.as_str()),
                ),
                None => ("—", None),
            };
            use ff_core::build_version::display_version_short;
            let cur_short = display_version_short(cur);
            let marker = match lat {
                Some(l) if display_version_short(l) == cur_short => "✓",
                Some(_) => "⚠",
                None => " ",
            };
            let disp = format!("{} {}", cur_short, marker);
            print!(" {:<14}", disp);
        }
        println!();
    }
    Ok(())
}
