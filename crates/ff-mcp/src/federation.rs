//! MCP federation + topology validation.
//!
//! Provides client-side discovery of external MCP endpoints configured in
//! `fleet.toml` (`[mcp.<name>]` with `client = true`), including:
//! - remote `tools/list` discovery
//! - optional `tools/call` proxying
//! - required/optional dependency and tool validation

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use ff_core::config::{FleetConfig, McpConfig};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

/// Tool discovered from a federated MCP endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedToolDefinition {
    pub service: String,
    pub endpoint: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Per-service federation probe status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedServiceStatus {
    pub name: String,
    pub endpoint: String,
    pub required: bool,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub required_dependencies: Vec<String>,
    pub optional_dependencies: Vec<String>,
    pub required_tools: Vec<String>,
    pub optional_tools: Vec<String>,
    pub discovered_tools: Vec<String>,
    pub error: Option<String>,
}

/// Topology validation report for federated MCP graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyValidationReport {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Snapshot of current MCP federation state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationSnapshot {
    pub services: Vec<FederatedServiceStatus>,
    pub tools: Vec<FederatedToolDefinition>,
    pub topology: TopologyValidationReport,
    pub scanned_at: String,
}

#[derive(Debug, Clone)]
struct FederationTarget {
    name: String,
    endpoint: String,
    timeout_secs: u64,
    cfg: McpConfig,
}

/// Discover all federated MCP services and validate required/optional topology.
pub async fn collect_federation_snapshot(
    config: &FleetConfig,
    default_timeout_secs: u64,
) -> FederationSnapshot {
    let targets = federation_targets(config, default_timeout_secs);
    if targets.is_empty() {
        return FederationSnapshot {
            services: vec![],
            tools: vec![],
            topology: TopologyValidationReport {
                valid: true,
                errors: vec![],
                warnings: vec!["no MCP client federation targets configured".to_string()],
            },
            scanned_at: Utc::now().to_rfc3339(),
        };
    }

    let mut services = Vec::with_capacity(targets.len());
    let mut all_tools = Vec::new();

    for target in targets {
        let probe_started = std::time::Instant::now();
        let list_result = tools_list_for_target(&target).await;
        let latency_ms = Some(probe_started.elapsed().as_millis() as u64);

        match list_result {
            Ok(tools) => {
                let mut discovered_names = Vec::new();
                for tool in &tools {
                    discovered_names.push(tool.name.clone());
                }

                all_tools.extend(tools);

                services.push(FederatedServiceStatus {
                    name: target.name,
                    endpoint: target.endpoint,
                    required: target.cfg.required.unwrap_or(false),
                    reachable: true,
                    latency_ms,
                    required_dependencies: target.cfg.depends_on,
                    optional_dependencies: target.cfg.optional_depends_on,
                    required_tools: target.cfg.required_tools,
                    optional_tools: target.cfg.optional_tools,
                    discovered_tools: discovered_names,
                    error: None,
                });
            }
            Err(err) => {
                if target.cfg.required.unwrap_or(false) {
                    warn!(
                        service = %target.name,
                        endpoint = %target.endpoint,
                        error = %err,
                        "required federated MCP probe failed"
                    );
                } else {
                    debug!(
                        service = %target.name,
                        endpoint = %target.endpoint,
                        error = %err,
                        "optional federated MCP probe failed"
                    );
                }

                services.push(FederatedServiceStatus {
                    name: target.name,
                    endpoint: target.endpoint,
                    required: target.cfg.required.unwrap_or(false),
                    reachable: false,
                    latency_ms,
                    required_dependencies: target.cfg.depends_on,
                    optional_dependencies: target.cfg.optional_depends_on,
                    required_tools: target.cfg.required_tools,
                    optional_tools: target.cfg.optional_tools,
                    discovered_tools: vec![],
                    error: Some(err),
                });
            }
        }
    }

    let topology = validate_topology(&services);

    FederationSnapshot {
        services,
        tools: all_tools,
        topology,
        scanned_at: Utc::now().to_rfc3339(),
    }
}

/// Convenience: list only federated tools.
pub async fn list_federated_tools(
    config: &FleetConfig,
    default_timeout_secs: u64,
) -> Vec<FederatedToolDefinition> {
    collect_federation_snapshot(config, default_timeout_secs)
        .await
        .tools
}

/// Proxy a tool call to the first federated MCP endpoint that exposes it.
pub async fn call_federated_tool(
    config: &FleetConfig,
    tool_name: &str,
    arguments: Option<Value>,
    default_timeout_secs: u64,
) -> Result<Value, String> {
    let targets = federation_targets(config, default_timeout_secs);
    if targets.is_empty() {
        return Err("no federated MCP targets configured".to_string());
    }

    for target in targets {
        let tools = match tools_list_for_target(&target).await {
            Ok(tools) => tools,
            Err(err) => {
                debug!(service = %target.name, error = %err, "skipping unreachable federated target");
                continue;
            }
        };

        if !tools.iter().any(|tool| tool.name == tool_name) {
            continue;
        }

        let timeout = Duration::from_secs(target.timeout_secs.max(1));
        let result = jsonrpc_request(
            &target.endpoint,
            timeout,
            "tools/call",
            Some(json!({
                "name": tool_name,
                "arguments": arguments.clone().unwrap_or_else(|| json!({}))
            })),
        )
        .await?;

        return Ok(parse_tools_call_result(
            &target.name,
            &target.endpoint,
            tool_name,
            result,
        ));
    }

    Err(format!("federated tool '{tool_name}' not found"))
}

fn validate_topology(services: &[FederatedServiceStatus]) -> TopologyValidationReport {
    let by_name: HashMap<&str, &FederatedServiceStatus> = services
        .iter()
        .map(|service| (service.name.as_str(), service))
        .collect();

    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for service in services {
        if service.required && !service.reachable {
            errors.push(format!(
                "required MCP service '{}' is unreachable ({})",
                service.name, service.endpoint
            ));
        }

        for dep in &service.required_dependencies {
            match by_name.get(dep.as_str()) {
                Some(dep_status) if dep_status.reachable => {}
                Some(dep_status) => errors.push(format!(
                    "service '{}' requires dependency '{}' but it is unreachable ({})",
                    service.name, dep, dep_status.endpoint
                )),
                None => errors.push(format!(
                    "service '{}' requires missing dependency '{}': not configured",
                    service.name, dep
                )),
            }
        }

        for dep in &service.optional_dependencies {
            match by_name.get(dep.as_str()) {
                Some(dep_status) if dep_status.reachable => {}
                Some(dep_status) => warnings.push(format!(
                    "service '{}' optional dependency '{}' is unreachable ({})",
                    service.name, dep, dep_status.endpoint
                )),
                None => warnings.push(format!(
                    "service '{}' optional dependency '{}' is not configured",
                    service.name, dep
                )),
            }
        }

        if service.reachable {
            let discovered: HashSet<&str> = service
                .discovered_tools
                .iter()
                .map(String::as_str)
                .collect();

            for required_tool in &service.required_tools {
                if !discovered.contains(required_tool.as_str()) {
                    errors.push(format!(
                        "service '{}' missing required tool '{}'",
                        service.name, required_tool
                    ));
                }
            }

            for optional_tool in &service.optional_tools {
                if !discovered.contains(optional_tool.as_str()) {
                    warnings.push(format!(
                        "service '{}' missing optional tool '{}'",
                        service.name, optional_tool
                    ));
                }
            }
        }
    }

    TopologyValidationReport {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

fn federation_targets(config: &FleetConfig, default_timeout_secs: u64) -> Vec<FederationTarget> {
    let mut targets = Vec::new();

    for (name, cfg) in &config.mcp {
        if cfg.client != Some(true) {
            continue;
        }

        let Some(endpoint) = resolve_endpoint(name, cfg) else {
            continue;
        };

        targets.push(FederationTarget {
            name: name.clone(),
            endpoint,
            timeout_secs: cfg
                .request_timeout_secs
                .unwrap_or(default_timeout_secs)
                .max(1),
            cfg: cfg.clone(),
        });
    }

    targets.sort_by(|a, b| a.name.cmp(&b.name));
    targets
}

fn resolve_endpoint(_name: &str, cfg: &McpConfig) -> Option<String> {
    if let Some(endpoint) = cfg.endpoint.as_ref().filter(|s| !s.trim().is_empty()) {
        return Some(normalize_endpoint(endpoint));
    }

    cfg.port.map(|port| format!("http://127.0.0.1:{port}/mcp"))
}

fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };

    match Url::parse(&with_scheme) {
        Ok(mut url) => {
            if url.path().is_empty() || url.path() == "/" {
                url.set_path("/mcp");
            }
            url.to_string()
        }
        Err(_) => with_scheme,
    }
}

async fn tools_list_for_target(
    target: &FederationTarget,
) -> Result<Vec<FederatedToolDefinition>, String> {
    let timeout = Duration::from_secs(target.timeout_secs.max(1));

    // Best-effort initialize handshake.
    let _ = jsonrpc_request(
        &target.endpoint,
        timeout,
        "initialize",
        Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "clientInfo": {
                "name": "forgefleetd",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
    )
    .await;

    let result = jsonrpc_request(&target.endpoint, timeout, "tools/list", None).await?;

    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "'tools/list' response missing tools array for {}",
                target.name
            )
        })?;

    let mut parsed = Vec::new();
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };

        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

        parsed.push(FederatedToolDefinition {
            service: target.name.clone(),
            endpoint: target.endpoint.clone(),
            name: name.to_string(),
            description,
            input_schema,
        });
    }

    Ok(parsed)
}

fn parse_tools_call_result(service: &str, endpoint: &str, tool_name: &str, result: Value) -> Value {
    if let Some(text) = result.pointer("/content/0/text").and_then(Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            return json!({
                "federated": true,
                "service": service,
                "endpoint": endpoint,
                "tool": tool_name,
                "result": parsed,
            });
        }

        return json!({
            "federated": true,
            "service": service,
            "endpoint": endpoint,
            "tool": tool_name,
            "result": text,
        });
    }

    json!({
        "federated": true,
        "service": service,
        "endpoint": endpoint,
        "tool": tool_name,
        "result": result,
    })
}

async fn jsonrpc_request(
    endpoint: &str,
    timeout: Duration,
    method: &str,
    params: Option<Value>,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": "ff-mcp-federation",
        "method": method,
        "params": params,
    });

    let response = client
        .post(endpoint)
        .json(&request)
        .send()
        .await
        .map_err(|e| format!("request failed for {endpoint}: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} from {endpoint}", status.as_u16()));
    }

    let payload: Value = response
        .json()
        .await
        .map_err(|e| format!("invalid JSON-RPC payload from {endpoint}: {e}"))?;

    if let Some(error) = payload.get("error") {
        return Err(format!(
            "JSON-RPC error from {endpoint} for method '{method}': {error}"
        ));
    }

    Ok(payload.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_endpoint_adds_scheme_and_mcp_path() {
        assert_eq!(
            normalize_endpoint("127.0.0.1:5000"),
            "http://127.0.0.1:5000/mcp"
        );
        assert_eq!(
            normalize_endpoint("https://example.com"),
            "https://example.com/mcp"
        );
        assert_eq!(
            normalize_endpoint("https://example.com/custom/mcp"),
            "https://example.com/custom/mcp"
        );
    }

    #[test]
    fn topology_flags_missing_required_dependency() {
        let services = vec![
            FederatedServiceStatus {
                name: "alpha".to_string(),
                endpoint: "http://alpha/mcp".to_string(),
                required: true,
                reachable: true,
                latency_ms: Some(3),
                required_dependencies: vec!["beta".to_string()],
                optional_dependencies: vec![],
                required_tools: vec![],
                optional_tools: vec![],
                discovered_tools: vec![],
                error: None,
            },
            FederatedServiceStatus {
                name: "beta".to_string(),
                endpoint: "http://beta/mcp".to_string(),
                required: false,
                reachable: false,
                latency_ms: Some(10),
                required_dependencies: vec![],
                optional_dependencies: vec![],
                required_tools: vec![],
                optional_tools: vec![],
                discovered_tools: vec![],
                error: Some("offline".to_string()),
            },
        ];

        let report = validate_topology(&services);
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("requires dependency 'beta'"))
        );
    }
}
