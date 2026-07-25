//! Fixed-suite software-engineering capability evaluation.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Subcommand;
use ff_agent::fleet_oneshot::fleet_oneshot_exact_model;
use ff_core::config::FleetConfig;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

const SUITE: &str = "forge-fleet-v1";

#[derive(Debug, Clone, Subcommand)]
pub enum BenchCommand {
    /// Evaluate one exact model deployment and persist its resolve rate.
    Run {
        #[arg(long)]
        model: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy)]
struct BenchTask {
    id: &'static str,
    summary: &'static str,
    broken: &'static str,
    #[allow(dead_code)]
    fixed: &'static str,
    tests: &'static str,
}

const TASKS: &[BenchTask] = &[
    BenchTask {
        id: "lease-boundary",
        summary: "stale leases expire at the boundary",
        broken: "fn lease_stale(age_secs: u64, limit: u64) -> bool { age_secs > limit }",
        fixed: "fn lease_stale(age_secs: u64, limit: u64) -> bool { age_secs >= limit }",
        tests: "#[test] fn t(){assert!(lease_stale(30,30));assert!(!lease_stale(29,30));}",
    },
    BenchTask {
        id: "slot-cap",
        summary: "capacity never underflows",
        broken: "fn free_slots(total:u32,used:u32)->u32{total-used}",
        fixed: "fn free_slots(total:u32,used:u32)->u32{total.saturating_sub(used)}",
        tests: "#[test] fn t(){assert_eq!(free_slots(2,5),0);assert_eq!(free_slots(5,2),3);}",
    },
    BenchTask {
        id: "retry-cap",
        summary: "retry cap is exclusive",
        broken: "fn may_retry(attempt:u8,cap:u8)->bool{attempt<=cap}",
        fixed: "fn may_retry(attempt:u8,cap:u8)->bool{attempt<cap}",
        tests: "#[test] fn t(){assert!(may_retry(2,3));assert!(!may_retry(3,3));}",
    },
    BenchTask {
        id: "model-prefix",
        summary: "strip only a leading local prefix",
        broken: "fn model_id(engine:&str)->&str{engine.trim_start_matches(\"local\")}",
        fixed: "fn model_id(engine:&str)->&str{engine.strip_prefix(\"local:\").unwrap_or(engine)}",
        tests: "#[test] fn t(){assert_eq!(model_id(\"local:qwen\"),\"qwen\");assert_eq!(model_id(\"localhost\"),\"localhost\");}",
    },
    BenchTask {
        id: "ctx-slots",
        summary: "effective context handles zero slots",
        broken: "fn usable_ctx(ctx:u32,slots:u32)->u32{ctx/slots}",
        fixed: "fn usable_ctx(ctx:u32,slots:u32)->u32{ctx/slots.max(1)}",
        tests: "#[test] fn t(){assert_eq!(usable_ctx(8192,0),8192);assert_eq!(usable_ctx(8192,2),4096);}",
    },
    BenchTask {
        id: "approve-rate",
        summary: "empty reviews have no score",
        broken: "fn approve_rate(ok:u32,total:u32)->Option<f64>{Some(ok as f64/total as f64)}",
        fixed: "fn approve_rate(ok:u32,total:u32)->Option<f64>{(total!=0).then(||ok as f64/total as f64)}",
        tests: "#[test] fn t(){assert_eq!(approve_rate(0,0),None);assert_eq!(approve_rate(1,2),Some(0.5));}",
    },
    BenchTask {
        id: "endpoint-join",
        summary: "endpoint paths get one slash",
        broken: "fn chat_url(base:&str)->String{format!(\"{base}/v1/chat/completions\")}",
        fixed: "fn chat_url(base:&str)->String{format!(\"{}/v1/chat/completions\",base.trim_end_matches('/'))}",
        tests: "#[test] fn t(){assert_eq!(chat_url(\"http://node/\"),\"http://node/v1/chat/completions\");}",
    },
    BenchTask {
        id: "verdict-case",
        summary: "approval parsing is case insensitive",
        broken: "fn approved(verdict:&str)->bool{verdict==\"approve\"}",
        fixed: "fn approved(verdict:&str)->bool{verdict.eq_ignore_ascii_case(\"approve\")}",
        tests: "#[test] fn t(){assert!(approved(\"APPROVE\"));assert!(!approved(\"reject\"));}",
    },
    BenchTask {
        id: "backoff-shift",
        summary: "backoff shift is bounded",
        broken: "fn backoff(base:u64,attempt:u32)->u64{base*(1<<attempt)}",
        fixed: "fn backoff(base:u64,attempt:u32)->u64{base.saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX))}",
        tests: "#[test] fn t(){assert_eq!(backoff(2,3),16);assert_eq!(backoff(2,80),u64::MAX);}",
    },
    BenchTask {
        id: "error-redact",
        summary: "tokens are redacted without panicking",
        broken: "fn redact(s:&str)->String{let(a,_)=s.split_once(\"token=\").unwrap();format!(\"{a}token=<redacted>\")}",
        fixed: "fn redact(s:&str)->String{s.split_once(\"token=\").map_or_else(||s.to_string(),|(a,_)|format!(\"{a}token=<redacted>\"))}",
        tests: "#[test] fn t(){assert_eq!(redact(\"safe\"),\"safe\");assert_eq!(redact(\"x token=secret\"),\"x token=<redacted>\");}",
    },
    BenchTask {
        id: "worker-sort",
        summary: "workers sort by load then name",
        broken: "fn worker_key(name:&str,load:u32)->(String,u32){(name.to_string(),load)}",
        fixed: "fn worker_key(name:&str,load:u32)->(u32,String){(load,name.to_string())}",
        tests: "#[test] fn t(){assert!(worker_key(\"z\",1)<worker_key(\"a\",2));}",
    },
    BenchTask {
        id: "duration-ceil",
        summary: "duration conversion rounds up",
        broken: "fn seconds(ms:u64)->u64{ms/1000}",
        fixed: "fn seconds(ms:u64)->u64{ms.saturating_add(999)/1000}",
        tests: "#[test] fn t(){assert_eq!(seconds(1),1);assert_eq!(seconds(1001),2);}",
    },
];

pub async fn run(command: BenchCommand) -> Result<()> {
    let BenchCommand::Run { model, json } = command;
    let pool = connect().await?;
    ff_agent::fleet_oneshot::resolve_route_candidate(&pool, &model)
        .await
        .with_context(|| format!("model `{model}` is not deployed and healthy"))?;
    let started = Instant::now();
    let mut resolved = 0usize;
    let mut results = Vec::with_capacity(TASKS.len());
    for (index, task) in TASKS.iter().enumerate() {
        let prompt = format!(
            "Fix this isolated Rust bug: {}.\nReturn ONLY the corrected function, no markdown or explanation.\n\n{}",
            task.summary, task.broken
        );
        let answer =
            fleet_oneshot_exact_model(&pool, &prompt, &model, Some(Duration::from_secs(180)))
                .await
                .with_context(|| format!("infrastructure failure on task `{}`", task.id))?;
        let (passed, error) = evaluate(&answer.text, task);
        resolved += usize::from(passed);
        results.push(json!({"task_id":task.id,"resolved":passed,"error":error}));
        if !json {
            println!(
                "[{}/{}] {}  {}",
                index + 1,
                TASKS.len(),
                if passed { "RESOLVED" } else { "FAILED" },
                task.id
            );
        }
    }
    let rate = resolved as f64 / TASKS.len() as f64;
    let elapsed = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    sqlx::query(
        "INSERT INTO ff_bench_results
         (model_id,suite,suite_version,resolved_tasks,total_tasks,resolve_rate,task_results,duration_ms)
         VALUES($1,$2,1,$3,$4,$5,$6,$7)",
    )
    .bind(&model)
    .bind(SUITE)
    .bind(resolved as i32)
    .bind(TASKS.len() as i32)
    .bind(rate)
    .bind(json!(results))
    .bind(elapsed)
    .execute(&pool)
    .await
    .context("persist ff-bench result (have migrations been applied?)")?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "model":model,"suite":SUITE,"resolved":resolved,
                "total":TASKS.len(),"resolve_rate":rate,"duration_ms":elapsed
            }))?
        );
    } else {
        println!(
            "\n{model}: {resolved}/{} resolved ({:.1}%)",
            TASKS.len(),
            rate * 100.0
        );
    }
    Ok(())
}

fn evaluate(answer: &str, task: &BenchTask) -> (bool, Option<String>) {
    let temp = match tempfile::tempdir() {
        Ok(value) => value,
        Err(error) => return (false, Some(error.to_string())),
    };
    let answer = answer.trim();
    let code = answer
        .strip_prefix("```")
        .map(|value| {
            value
                .strip_prefix("rust")
                .unwrap_or(value)
                .trim_start()
                .strip_suffix("```")
                .unwrap_or(value)
                .trim()
        })
        .unwrap_or(answer);
    let source = temp.path().join("task.rs");
    let binary = temp.path().join("task-test");
    if let Err(error) = std::fs::write(&source, format!("{code}\n{}\n", task.tests)) {
        return (false, Some(error.to_string()));
    }
    match std::process::Command::new("rustc")
        .args(["--edition=2024", "--test"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            return (
                false,
                Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
            );
        }
        Err(error) => return (false, Some(error.to_string())),
    }
    match std::process::Command::new(binary).output() {
        Ok(output) if output.status.success() => (true, None),
        Ok(output) => (
            false,
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        ),
        Err(error) => (false, Some(error.to_string())),
    }
}

async fn connect() -> Result<sqlx::PgPool> {
    let path = dirs::home_dir()
        .context("no home dir")?
        .join(".forgefleet/fleet.toml");
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let config: FleetConfig = toml::from_str(&raw).context("parse fleet.toml")?;
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(config.database.url.trim())
        .await
        .context("connect postgres")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_is_fixed_and_behaviorally_scored() {
        assert_eq!(TASKS.len(), 12);
        assert!(TASKS.iter().all(|task| task.broken != task.fixed));
        assert!(evaluate(TASKS[0].fixed, &TASKS[0]).0);
        assert!(!evaluate(TASKS[0].broken, &TASKS[0]).0);
    }
}
