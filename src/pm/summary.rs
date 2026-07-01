use std::collections::BTreeMap;

#[derive(sqlx::FromRow)]
struct WorkItemRow {
    status: String,
}

#[derive(sqlx::FromRow)]
struct FleetPrRow {
    pr_url: String,
}

async fn fetch_work_items() -> Result<Vec<WorkItemRow>, Box<dyn std::error::Error>> {
    let pool = ff_agent::fleet_info::get_fleet_pool().await?;

    let rows = sqlx::query_as::<_, WorkItemRow>(
        "SELECT status
           FROM work_items
          ORDER BY created_at DESC",
    )
    .fetch_all(&pool)
    .await?;

    Ok(rows)
}

async fn fetch_open_fleet_prs() -> Result<Vec<FleetPrRow>, Box<dyn std::error::Error>> {
    let pool = ff_agent::fleet_info::get_fleet_pool().await?;

    let rows = sqlx::query_as::<_, FleetPrRow>(
        "SELECT pr_url
           FROM project_branches
          WHERE pr_state = 'open'
            AND pr_url IS NOT NULL
            AND btrim(pr_url) <> ''
          ORDER BY pr_url",
    )
    .fetch_all(&pool)
    .await?;

    Ok(rows)
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let work_items = fetch_work_items().await?;
    let prs = fetch_open_fleet_prs().await?;

    let mut status_counts = BTreeMap::<String, usize>::new();
    for item in work_items {
        *status_counts.entry(item.status).or_insert(0) += 1;
    }

    let mut parts = Vec::with_capacity(status_counts.len() + 2);
    parts.push("Summary:".to_string());
    parts.extend(
        status_counts
            .into_iter()
            .map(|(status, count)| format!("{status}={count}")),
    );
    parts.push(format!("PRs:{}", prs.len()));

    println!("{}", parts.join(" "));
    Ok(())
}
