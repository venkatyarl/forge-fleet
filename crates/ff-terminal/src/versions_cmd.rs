use crate::truncate_for_col;
use anyhow::Result;
use ff_core::build_version::display_version_short;
use serde_json::{Value, json};

/// Drift status for one (tool, node) cell, kept stable for scripting.
/// `current`  — current == latest (short form match)
/// `drift`    — current != latest (an upgrade is available)
/// `unknown`  — no `latest` recorded yet (can't compare)
/// `missing`  — node has no entry for this tool at all
fn cell_status(cur: Option<&str>, lat: Option<&str>) -> &'static str {
    match (cur, lat) {
        (None, _) => "missing",
        (Some(_), None) => "unknown",
        (Some(c), Some(l)) => {
            if display_version_short(c) == display_version_short(l) {
                "current"
            } else {
                "drift"
            }
        }
    }
}

/// Build the lossless JSON projection: one object per node, each with a
/// `tools` map of tool → {current, latest, current_short, latest_short, status}.
/// Pure (no DB/clock), decoupled from `FleetNodeRow` so it's unit-testable.
fn versions_json<'a>(
    filtered: impl IntoIterator<Item = (&'a str, &'a Value)>,
    all_keys: &[String],
) -> Value {
    let nodes: Vec<Value> = filtered
        .into_iter()
        .map(|(name, tooling)| {
            let mut tools = serde_json::Map::new();
            for k in all_keys {
                let cell = tooling.get(k).and_then(|c| c.as_object());
                // A tool absent from this node's map is omitted entirely so
                // consumers can distinguish "not installed" from "no latest yet".
                let Some(obj) = cell else { continue };
                let cur = obj.get("current").and_then(|v| v.as_str());
                let lat = obj.get("latest").and_then(|v| v.as_str());
                tools.insert(
                    k.clone(),
                    json!({
                        "current": cur,
                        "latest": lat,
                        "current_short": cur.map(display_version_short),
                        "latest_short": lat.map(display_version_short),
                        "status": cell_status(cur, lat),
                    }),
                );
            }
            json!({ "node": name, "tools": Value::Object(tools) })
        })
        .collect();
    Value::Array(nodes)
}

pub async fn handle_versions(node_filter: Option<String>, json_out: bool) -> Result<()> {
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

    let mut all_keys_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for n in &filtered {
        if let Some(obj) = n.tooling.as_object() {
            for k in obj.keys() {
                all_keys_set.insert(k.clone());
            }
        }
    }
    let all_keys: Vec<String> = all_keys_set.into_iter().collect();

    if json_out {
        // Lossless structured form; empty tooling → empty per-node maps (still valid).
        let out = versions_json(
            filtered.iter().map(|n| (n.name.as_str(), &n.tooling)),
            &all_keys,
        );
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_status_classifies() {
        let a = "ff 2026.6.13_1 (pushed aaaaaaa111)";
        let b = "ff 2026.6.13_2 (pushed bbbbbbb222)";
        assert_eq!(cell_status(None, None), "missing");
        assert_eq!(cell_status(Some(a), None), "unknown");
        // Same SHA (different build_count/date) → current.
        assert_eq!(
            cell_status(Some(a), Some("ff 2026.6.14_9 (pushed aaaaaaa111)")),
            "current"
        );
        assert_eq!(cell_status(Some(a), Some(b)), "drift");
    }

    #[test]
    fn json_projection_is_lossless_and_per_node() {
        let cur = "2026.6.13_1 (pushed aaaaaaa111)";
        let newer = "2026.6.13_2 (pushed bbbbbbb222)";
        let taylor = json!({
            "ff": {"current": cur, "latest": cur},
            "forgefleetd": {"current": cur, "latest": newer},
        });
        // marcus is missing forgefleetd and has no `latest` for ff (unknown).
        let marcus = json!({ "ff": {"current": cur} });
        let filtered = vec![("taylor", &taylor), ("marcus", &marcus)];
        let all_keys = vec!["ff".to_string(), "forgefleetd".to_string()];
        let out = versions_json(filtered, &all_keys);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // taylor: ff current, forgefleetd drift, full current/latest preserved.
        assert_eq!(arr[0]["node"], "taylor");
        assert_eq!(arr[0]["tools"]["ff"]["status"], "current");
        assert_eq!(arr[0]["tools"]["forgefleetd"]["status"], "drift");
        assert_eq!(arr[0]["tools"]["forgefleetd"]["current"], cur);
        assert_eq!(arr[0]["tools"]["forgefleetd"]["latest_short"], "bbbbbbb2");

        // marcus: ff unknown (no latest), forgefleetd omitted (not installed).
        assert_eq!(arr[1]["node"], "marcus");
        assert_eq!(arr[1]["tools"]["ff"]["status"], "unknown");
        assert!(arr[1]["tools"].get("forgefleetd").is_none());
    }
}
