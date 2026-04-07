//! Database tool — run SQL queries against PostgreSQL, SQLite, MySQL.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct DatabaseQueryTool;

#[async_trait]
impl AgentTool for DatabaseQueryTool {
    fn name(&self) -> &str { "DatabaseQuery" }
    fn description(&self) -> &str { "Run SQL queries against databases. Supports PostgreSQL (psql), SQLite (sqlite3), and MySQL (mysql). Use for data inspection, schema exploration, and data operations." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"SQL query to execute"},
            "database":{"type":"string","description":"Database connection: file path for SQLite, or connection string for Postgres/MySQL"},
            "db_type":{"type":"string","enum":["sqlite","postgres","mysql","auto"],"description":"Database type (default: auto-detect)"}
        },"required":["query","database"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        let database = input.get("database").and_then(Value::as_str).unwrap_or("");
        let db_type = input.get("db_type").and_then(Value::as_str).unwrap_or("auto");

        if query.is_empty() || database.is_empty() { return AgentToolResult::err("'query' and 'database' required"); }

        // Block destructive queries unless explicitly allowed
        let lower = query.to_ascii_lowercase();
        if lower.contains("drop ") || lower.contains("truncate ") || lower.contains("delete from") && !lower.contains("where") {
            return AgentToolResult::err("Destructive query blocked. Add a WHERE clause or use Bash for explicit operations.");
        }

        let detected = if db_type != "auto" { db_type.to_string() }
            else if database.ends_with(".db") || database.ends_with(".sqlite") || database.ends_with(".sqlite3") { "sqlite".into() }
            else if database.starts_with("postgres") || database.starts_with("postgresql") { "postgres".into() }
            else if database.starts_with("mysql") { "mysql".into() }
            else { "sqlite".into() };

        let result = match detected.as_str() {
            "sqlite" => {
                let db_path = if std::path::Path::new(database).is_absolute() { database.to_string() } else { ctx.working_dir.join(database).to_string_lossy().to_string() };
                Command::new("sqlite3").args(["-header", "-column", &db_path, query]).output().await
            }
            "postgres" => {
                Command::new("psql").args([database, "-c", query]).output().await
            }
            "mysql" => {
                Command::new("mysql").args(["-e", query, database]).output().await
            }
            _ => return AgentToolResult::err(format!("Unknown db type: {detected}")),
        };

        match result {
            Ok(out) if out.status.success() => {
                AgentToolResult::ok(truncate_output(&String::from_utf8_lossy(&out.stdout), MAX_TOOL_RESULT_CHARS))
            }
            Ok(out) => AgentToolResult::err(truncate_output(&format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)), MAX_TOOL_RESULT_CHARS)),
            Err(e) => AgentToolResult::err(format!("Database command failed: {e}")),
        }
    }
}
