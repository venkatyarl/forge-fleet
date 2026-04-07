//! Crypto & security tools — hashing, encryption, password generation.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult};

pub struct HashGeneratorTool;

#[async_trait]
impl AgentTool for HashGeneratorTool {
    fn name(&self) -> &str { "HashGenerator" }
    fn description(&self) -> &str { "Generate cryptographic hashes: SHA256, SHA512, MD5, SHA1. Hash strings or files." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "input":{"type":"string","description":"Text to hash"},
            "file":{"type":"string","description":"File path to hash (alternative to input)"},
            "algorithm":{"type":"string","enum":["sha256","sha512","md5","sha1"],"description":"Hash algorithm (default: sha256)"}
        }})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let algo = input.get("algorithm").and_then(Value::as_str).unwrap_or("sha256");
        let cmd_name = match algo { "sha256" => "shasum -a 256", "sha512" => "shasum -a 512", "md5" => "md5sum", "sha1" => "shasum", _ => "shasum -a 256" };

        if let Some(text) = input.get("input").and_then(Value::as_str) {
            let cmd = format!("echo -n '{}' | {}", text.replace('\'', "'\"'\"'"), cmd_name);
            match Command::new("bash").arg("-c").arg(&cmd).output().await {
                Ok(o) => AgentToolResult::ok(format!("{} ({}): {}", algo.to_uppercase(), "string", String::from_utf8_lossy(&o.stdout).trim())),
                Err(e) => AgentToolResult::err(format!("Hash failed: {e}")),
            }
        } else if let Some(file) = input.get("file").and_then(Value::as_str) {
            let path = if std::path::Path::new(file).is_absolute() { file.to_string() } else { ctx.working_dir.join(file).to_string_lossy().to_string() };
            let cmd = format!("{} '{}'", cmd_name, path);
            match Command::new("bash").arg("-c").arg(&cmd).output().await {
                Ok(o) => AgentToolResult::ok(format!("{} (file): {}", algo.to_uppercase(), String::from_utf8_lossy(&o.stdout).trim())),
                Err(e) => AgentToolResult::err(format!("Hash failed: {e}")),
            }
        } else {
            AgentToolResult::err("Provide 'input' (text) or 'file' (path)".to_string())
        }
    }
}

pub struct PasswordGenTool;

#[async_trait]
impl AgentTool for PasswordGenTool {
    fn name(&self) -> &str { "PasswordGen" }
    fn description(&self) -> &str { "Generate secure random passwords with configurable length and character sets." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "length":{"type":"number","description":"Password length (default: 24)"},
            "count":{"type":"number","description":"Number of passwords (default: 1)"},
            "no_special":{"type":"boolean","description":"Exclude special characters (default: false)"},
            "format":{"type":"string","enum":["random","passphrase","pin"],"description":"Format (default: random)"}
        }})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let length = input.get("length").and_then(Value::as_u64).unwrap_or(24) as usize;
        let count = input.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;
        let format = input.get("format").and_then(Value::as_str).unwrap_or("random");

        let mut passwords = Vec::new();
        for _ in 0..count {
            let cmd = match format {
                "passphrase" => format!("LC_ALL=C tr -dc 'a-z' < /dev/urandom | fold -w 5 | head -6 | paste -sd'-' -"),
                "pin" => format!("LC_ALL=C tr -dc '0-9' < /dev/urandom | head -c {length}"),
                _ => {
                    let charset = if input.get("no_special").and_then(Value::as_bool).unwrap_or(false) {
                        "a-zA-Z0-9"
                    } else {
                        "a-zA-Z0-9!@#$%^&*()-_=+"
                    };
                    format!("LC_ALL=C tr -dc '{}' < /dev/urandom | head -c {}", charset, length)
                }
            };
            if let Ok(o) = Command::new("bash").arg("-c").arg(&cmd).output().await {
                passwords.push(String::from_utf8_lossy(&o.stdout).trim().to_string());
            }
        }
        AgentToolResult::ok(passwords.join("\n"))
    }
}

pub struct TextTransformTool;

#[async_trait]
impl AgentTool for TextTransformTool {
    fn name(&self) -> &str { "TextTransform" }
    fn description(&self) -> &str { "Transform text: base64 encode/decode, URL encode/decode, JSON format/minify, count words/lines/chars." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["base64_encode","base64_decode","url_encode","url_decode","json_format","json_minify","count","upper","lower","reverse"]},
            "input":{"type":"string","description":"Text to transform"}
        },"required":["action","input"]})
    }
    async fn execute(&self, input_val: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input_val.get("action").and_then(Value::as_str).unwrap_or("");
        let text = input_val.get("input").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() { return AgentToolResult::err("'input' required"); }

        match action {
            "base64_encode" => {
                match Command::new("bash").arg("-c").arg(format!("echo -n '{}' | base64", text.replace('\'', "'\"'\"'"))).output().await {
                    Ok(o) => AgentToolResult::ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
                    Err(e) => AgentToolResult::err(format!("base64 encode failed: {e}")),
                }
            }
            "base64_decode" => {
                match Command::new("bash").arg("-c").arg(format!("echo '{}' | base64 -d", text)).output().await {
                    Ok(o) => AgentToolResult::ok(String::from_utf8_lossy(&o.stdout).to_string()),
                    Err(e) => AgentToolResult::err(format!("base64 decode failed: {e}")),
                }
            }
            "url_encode" => {
                let encoded: String = text.chars().map(|c| match c {
                    'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
                    _ => format!("%{:02X}", c as u8),
                }).collect();
                AgentToolResult::ok(encoded)
            }
            "url_decode" => {
                let mut decoded = String::new();
                let mut chars = text.chars();
                while let Some(c) = chars.next() {
                    if c == '%' {
                        let hex: String = chars.by_ref().take(2).collect();
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) { decoded.push(byte as char); }
                    } else if c == '+' { decoded.push(' '); }
                    else { decoded.push(c); }
                }
                AgentToolResult::ok(decoded)
            }
            "json_format" => {
                match serde_json::from_str::<Value>(text) {
                    Ok(v) => AgentToolResult::ok(serde_json::to_string_pretty(&v).unwrap_or_default()),
                    Err(e) => AgentToolResult::err(format!("Invalid JSON: {e}")),
                }
            }
            "json_minify" => {
                match serde_json::from_str::<Value>(text) {
                    Ok(v) => AgentToolResult::ok(serde_json::to_string(&v).unwrap_or_default()),
                    Err(e) => AgentToolResult::err(format!("Invalid JSON: {e}")),
                }
            }
            "count" => {
                let lines = text.lines().count();
                let words = text.split_whitespace().count();
                let chars = text.chars().count();
                let bytes = text.len();
                AgentToolResult::ok(format!("Lines: {lines}\nWords: {words}\nChars: {chars}\nBytes: {bytes}"))
            }
            "upper" => AgentToolResult::ok(text.to_uppercase()),
            "lower" => AgentToolResult::ok(text.to_lowercase()),
            "reverse" => AgentToolResult::ok(text.chars().rev().collect::<String>()),
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

pub struct CalculatorTool;

#[async_trait]
impl AgentTool for CalculatorTool {
    fn name(&self) -> &str { "Calculator" }
    fn description(&self) -> &str { "Evaluate math expressions, unit conversions, and calculations. Uses bc or Python for computation." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "expression":{"type":"string","description":"Math expression to evaluate (e.g. '(100 * 1.08) / 12', 'sqrt(144)', '2^10')"}
        },"required":["expression"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let expr = input.get("expression").and_then(Value::as_str).unwrap_or("");
        if expr.is_empty() { return AgentToolResult::err("'expression' required"); }

        // Try Python first (most capable)
        let py_cmd = format!("python3 -c \"from math import *; print({})\"", expr.replace('"', "\\\""));
        if let Ok(out) = Command::new("bash").arg("-c").arg(&py_cmd).output().await {
            if out.status.success() {
                return AgentToolResult::ok(format!("{} = {}", expr, String::from_utf8_lossy(&out.stdout).trim()));
            }
        }

        // Fallback to bc
        let bc_cmd = format!("echo 'scale=6; {}' | bc -l", expr);
        match Command::new("bash").arg("-c").arg(&bc_cmd).output().await {
            Ok(out) if out.status.success() => {
                AgentToolResult::ok(format!("{} = {}", expr, String::from_utf8_lossy(&out.stdout).trim()))
            }
            _ => AgentToolResult::err(format!("Calculation failed. Expression: {expr}")),
        }
    }
}
