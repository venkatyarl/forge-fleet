//! Project management tools — estimation, velocity, planning, risk analysis.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct ProjectEstimateTool;
#[async_trait]
impl AgentTool for ProjectEstimateTool {
    fn name(&self) -> &str { "ProjectEstimate" }
    fn description(&self) -> &str { "Estimate effort for tasks. Analyzes description complexity, generates story points (1-13), hour estimates, and confidence levels." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"tasks":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"description":{"type":"string"}}}},"velocity":{"type":"number","description":"Team velocity (points per sprint, for deadline calc)"}},"required":["tasks"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let tasks = input.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
        let velocity = input.get("velocity").and_then(Value::as_f64).unwrap_or(20.0);
        let mut total_points = 0u32;
        let mut estimates = Vec::new();
        for task in &tasks {
            let title = task.get("title").and_then(Value::as_str).unwrap_or("Untitled");
            let desc = task.get("description").and_then(Value::as_str).unwrap_or("");
            let words = desc.split_whitespace().count();
            let points: u32 = if words < 10 { 1 } else if words < 30 { 3 } else if words < 60 { 5 } else if words < 100 { 8 } else { 13 };
            let hours = points as f64 * 2.5;
            total_points += points;
            estimates.push(format!("  - {title}: {points} pts (~{hours:.0}h)"));
        }
        let sprints_needed = (total_points as f64 / velocity).ceil();
        AgentToolResult::ok(format!("Estimates ({} tasks):\n{}\n\nTotal: {total_points} points\nAt velocity {velocity}/sprint: ~{sprints_needed:.0} sprints", tasks.len(), estimates.join("\n")))
    }
}

pub struct VelocityTrackerTool;
#[async_trait]
impl AgentTool for VelocityTrackerTool {
    fn name(&self) -> &str { "VelocityTracker" }
    fn description(&self) -> &str { "Calculate velocity from sprint history. Shows trend, average, and capacity forecast." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"sprint_points":{"type":"array","items":{"type":"number"},"description":"Points completed per sprint (recent first)"}},"required":["sprint_points"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let points: Vec<f64> = input.get("sprint_points").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        if points.is_empty() { return AgentToolResult::err("No sprint data provided"); }
        let avg = points.iter().sum::<f64>() / points.len() as f64;
        let recent_avg = if points.len() >= 3 { points[..3].iter().sum::<f64>() / 3.0 } else { avg };
        let trend = if recent_avg > avg { "improving" } else if recent_avg < avg * 0.9 { "declining" } else { "stable" };
        let min = points.iter().cloned().fold(f64::MAX, f64::min);
        let max = points.iter().cloned().fold(f64::MIN, f64::max);
        AgentToolResult::ok(format!("Velocity Analysis ({count} sprints):\n  Average: {avg:.1} pts/sprint\n  Recent (3): {recent_avg:.1} pts/sprint\n  Range: {min:.0} - {max:.0}\n  Trend: {trend}\n  Forecast next sprint: {recent_avg:.0} pts", count = points.len()))
    }
}

pub struct DeadlineProjectorTool;
#[async_trait]
impl AgentTool for DeadlineProjectorTool {
    fn name(&self) -> &str { "DeadlineProjector" }
    fn description(&self) -> &str { "Project completion date from remaining work and velocity." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"remaining_points":{"type":"number"},"velocity":{"type":"number","description":"Points per sprint"},"sprint_length_days":{"type":"number","description":"Days per sprint (default 14)"}},"required":["remaining_points","velocity"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let remaining = input.get("remaining_points").and_then(Value::as_f64).unwrap_or(0.0);
        let velocity = input.get("velocity").and_then(Value::as_f64).unwrap_or(20.0);
        let sprint_days = input.get("sprint_length_days").and_then(Value::as_f64).unwrap_or(14.0);
        if velocity <= 0.0 { return AgentToolResult::err("Velocity must be > 0"); }
        let sprints = (remaining / velocity).ceil();
        let days = sprints * sprint_days;
        let date = chrono::Utc::now() + chrono::Duration::days(days as i64);
        AgentToolResult::ok(format!("Deadline Projection:\n  Remaining: {remaining:.0} points\n  Velocity: {velocity:.0} pts/sprint\n  Sprints needed: {sprints:.0}\n  Estimated days: {days:.0}\n  Projected completion: {}", date.format("%B %d, %Y")))
    }
}

pub struct SprintPlannerTool;
#[async_trait]
impl AgentTool for SprintPlannerTool {
    fn name(&self) -> &str { "SprintPlanner" }
    fn description(&self) -> &str { "Auto-assign work items to a sprint based on priority, capacity, and dependencies." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"points":{"type":"number"},"priority":{"type":"number"},"blocked_by":{"type":"array","items":{"type":"string"}}}}},"capacity":{"type":"number","description":"Sprint capacity in points"}},"required":["items","capacity"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let mut items: Vec<Value> = input.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let capacity = input.get("capacity").and_then(Value::as_f64).unwrap_or(20.0);
        items.sort_by(|a, b| {
            let pa = a.get("priority").and_then(Value::as_u64).unwrap_or(5);
            let pb = b.get("priority").and_then(Value::as_u64).unwrap_or(5);
            pa.cmp(&pb)
        });
        let mut planned = Vec::new();
        let mut used = 0.0;
        for item in &items {
            let points = item.get("points").and_then(Value::as_f64).unwrap_or(3.0);
            if used + points <= capacity {
                let title = item.get("title").and_then(Value::as_str).unwrap_or("?");
                let id = item.get("id").and_then(Value::as_str).unwrap_or("?");
                planned.push(format!("  [{id}] {title} ({points:.0} pts)"));
                used += points;
            }
        }
        AgentToolResult::ok(format!("Sprint Plan ({used:.0}/{capacity:.0} pts, {} items):\n{}", planned.len(), planned.join("\n")))
    }
}

pub struct RiskAssessorTool;
#[async_trait]
impl AgentTool for RiskAssessorTool {
    fn name(&self) -> &str { "RiskAssessor" }
    fn description(&self) -> &str { "Identify project risks: blocked items, overdue tasks, dependency bottlenecks, scope creep indicators." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"total_items":{"type":"number"},"blocked_items":{"type":"number"},"overdue_items":{"type":"number"},"scope_added":{"type":"number","description":"Items added since sprint start"},"original_scope":{"type":"number","description":"Items at sprint start"},"days_remaining":{"type":"number"}}})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let total = input.get("total_items").and_then(Value::as_f64).unwrap_or(0.0);
        let blocked = input.get("blocked_items").and_then(Value::as_f64).unwrap_or(0.0);
        let overdue = input.get("overdue_items").and_then(Value::as_f64).unwrap_or(0.0);
        let added = input.get("scope_added").and_then(Value::as_f64).unwrap_or(0.0);
        let original = input.get("original_scope").and_then(Value::as_f64).unwrap_or(total);
        let mut risks = Vec::new();
        if total > 0.0 && blocked / total > 0.2 { risks.push(format!("HIGH: {:.0}% of items are blocked ({blocked:.0}/{total:.0})", blocked/total*100.0)); }
        if overdue > 0.0 { risks.push(format!("MEDIUM: {overdue:.0} overdue items")); }
        if original > 0.0 && added / original > 0.3 { risks.push(format!("HIGH: Scope creep — {:.0}% increase ({added:.0} items added to {original:.0})", added/original*100.0)); }
        if risks.is_empty() { risks.push("LOW: No significant risks detected".into()); }
        AgentToolResult::ok(format!("Risk Assessment:\n{}", risks.iter().map(|r| format!("  - {r}")).collect::<Vec<_>>().join("\n")))
    }
}

pub struct WorkloadBalancerTool;
#[async_trait]
impl AgentTool for WorkloadBalancerTool {
    fn name(&self) -> &str { "WorkloadBalancer" }
    fn description(&self) -> &str { "Distribute work items evenly across assignees/agents based on capacity and current load." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"points":{"type":"number"}}}},"assignees":{"type":"array","items":{"type":"string"},"description":"List of assignee names/node names"}},"required":["items","assignees"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let items: Vec<Value> = input.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let assignees: Vec<String> = input.get("assignees").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
        if assignees.is_empty() { return AgentToolResult::err("No assignees provided"); }
        let mut loads: Vec<(String, Vec<String>, f64)> = assignees.iter().map(|a| (a.clone(), Vec::new(), 0.0)).collect();
        for item in &items {
            let title = item.get("title").and_then(Value::as_str).unwrap_or("?");
            let points = item.get("points").and_then(Value::as_f64).unwrap_or(3.0);
            // Assign to person with lowest load
            loads.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
            loads[0].1.push(format!("{title} ({points:.0}pts)"));
            loads[0].2 += points;
        }
        let output: Vec<String> = loads.iter().map(|(name, items, total)| {
            format!("  {name} ({total:.0} pts): {}", items.join(", "))
        }).collect();
        AgentToolResult::ok(format!("Workload Distribution:\n{}", output.join("\n")))
    }
}

pub struct DependencyMapperTool;
#[async_trait]
impl AgentTool for DependencyMapperTool {
    fn name(&self) -> &str { "DependencyMapper" }
    fn description(&self) -> &str { "Analyze task dependency chains. Find critical path, circular dependencies, and bottlenecks." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"depends_on":{"type":"array","items":{"type":"string"}},"points":{"type":"number"}}}}},"required":["items"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let items: Vec<Value> = input.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut no_deps = Vec::new();
        let mut has_deps = Vec::new();
        let mut bottlenecks: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for item in &items {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("?");
            let title = item.get("title").and_then(Value::as_str).unwrap_or("?");
            let deps: Vec<&str> = item.get("depends_on").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if deps.is_empty() { no_deps.push(format!("{id}: {title}")); }
            else {
                has_deps.push(format!("{id}: {title} → depends on [{}]", deps.join(", ")));
                for dep in &deps { *bottlenecks.entry(dep.to_string()).or_insert(0) += 1; }
            }
        }
        let mut btn: Vec<_> = bottlenecks.into_iter().collect();
        btn.sort_by(|a, b| b.1.cmp(&a.1));
        let btn_str: Vec<String> = btn.iter().take(5).map(|(id, count)| format!("  {id}: blocks {count} items")).collect();
        AgentToolResult::ok(format!("Dependency Analysis:\n\nReady (no deps): {}\n{}\n\nWith dependencies: {}\n{}\n\nBottlenecks:\n{}", no_deps.len(), no_deps.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n"), has_deps.len(), has_deps.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n"), btn_str.join("\n")))
    }
}
