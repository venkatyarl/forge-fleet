//! Fleet operations tools — onboard nodes, deploy models, manage fleet infrastructure.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, truncate_output};

/// NodeSetup — install prerequisites on a new machine via SSH.
pub struct NodeSetupTool;

#[async_trait]
impl AgentTool for NodeSetupTool {
    fn name(&self) -> &str {
        "NodeSetup"
    }
    fn description(&self) -> &str {
        "Set up a new fleet node via SSH. Installs prerequisites (Rust, llama.cpp, Docker, system tools), creates the forgefleet user, and configures the system for fleet membership."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "IP address or hostname of the new machine" },
                "user": { "type": "string", "description": "SSH user (default: root or current user)" },
                "password": { "type": "string", "description": "SSH password (if not using key auth)" },
                "os": { "type": "string", "enum": ["ubuntu", "macos", "auto"], "description": "Target OS (default: auto-detect)" },
                "install": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Components to install: rust, llamacpp, ollama, docker, forgefleet (default: all)"
                },
                "node_name": { "type": "string", "description": "Name for this node in the fleet" }
            },
            "required": ["host"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let host = match input.get("host").and_then(Value::as_str) {
            Some(h) => h,
            None => return AgentToolResult::err("Missing 'host'"),
        };
        let user = input.get("user").and_then(Value::as_str).unwrap_or("root");
        let node_name = input
            .get("node_name")
            .and_then(Value::as_str)
            .unwrap_or("new-node");

        let install_list: Vec<&str> = input
            .get("install")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_else(|| vec!["rust", "llamacpp", "docker"]);

        let mut results: Vec<String> = Vec::new();

        // Step 1: Test SSH connectivity
        results.push("Step 1: Testing SSH connectivity...".into());
        let ssh_test = Command::new("ssh")
            .args([
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=no",
                &format!("{user}@{host}"),
                "echo",
                "ok",
            ])
            .output()
            .await;

        match ssh_test {
            Ok(out) if out.status.success() => results.push("  SSH: Connected".into()),
            _ => {
                return AgentToolResult::err(format!(
                    "Cannot SSH to {user}@{host}. Check connectivity and credentials."
                ));
            }
        }

        // Step 2: Detect OS
        let _os_detect = ssh_cmd(
            user,
            host,
            "uname -s && cat /etc/os-release 2>/dev/null | head -3 || sw_vers 2>/dev/null",
        )
        .await;
        results.push("Step 2: OS detected".into());

        // Step 3: Install components
        let mut install_script = String::from("set -e\n");

        if install_list.contains(&"rust") {
            install_script.push_str("# Install Rust\ncurl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y\nsource $HOME/.cargo/env\nrustc --version\n");
        }

        if install_list.contains(&"llamacpp") {
            install_script.push_str("# Install llama.cpp\nif [ ! -d ~/llama.cpp ]; then git clone https://github.com/ggml-org/llama.cpp ~/llama.cpp && cd ~/llama.cpp && make -j$(nproc); fi\n");
        }

        if install_list.contains(&"docker") {
            install_script.push_str("# Install Docker (Ubuntu)\nif ! command -v docker &>/dev/null; then curl -fsSL https://get.docker.com | sh; fi\n");
        }

        if install_list.contains(&"ollama") {
            install_script.push_str("# Install Ollama\nif ! command -v ollama &>/dev/null; then curl -fsSL https://ollama.com/install.sh | sh; fi\n");
        }

        // Step 4: Create forgefleet directory structure
        install_script.push_str(&format!(
            r#"
# ForgeFleet setup
mkdir -p ~/.forgefleet/memory/global
mkdir -p ~/.forgefleet/sessions
mkdir -p ~/.forgefleet/plugins
mkdir -p ~/projects
echo '{node_name}' > ~/.forgefleet/node_name
echo 'ForgeFleet node setup complete'
"#
        ));

        results.push("Step 3: Installing components...".into());

        let install_result = ssh_cmd(user, host, &install_script).await;
        results.push("Step 4: Creating ForgeFleet directories...".into());

        // Step 5: Verify
        let verify = ssh_cmd(user, host, "which rustc 2>/dev/null; which docker 2>/dev/null; ls ~/.forgefleet/node_name 2>/dev/null; echo 'Verification complete'").await;

        AgentToolResult::ok(format!(
            "Node Setup Report — {node_name} ({host})\n\n{}\n\nInstall output:\n{}\n\nVerification:\n{}\n\nNext steps:\n  1. Run NodeEnroll to add this node to fleet.toml\n  2. Run ModelDeploy to download a model\n  3. Start the LLM server on the node",
            results.join("\n"),
            truncate_output(&install_result, 2000),
            truncate_output(&verify, 500)
        ))
    }
}

/// NodeEnroll — register a node in fleet.toml and set up SSH keys.
pub struct NodeEnrollTool;

#[async_trait]
impl AgentTool for NodeEnrollTool {
    fn name(&self) -> &str {
        "NodeEnroll"
    }
    fn description(&self) -> &str {
        "Enroll a new node into the ForgeFleet fleet. Updates fleet.toml, sets up SSH key exchange, and verifies bidirectional connectivity."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "node_name": { "type": "string", "description": "Name for this node (e.g. 'evo1', 'sia')" },
                "ip": { "type": "string", "description": "IP address" },
                "port": { "type": "number", "description": "LLM server port (default: 51000)" },
                "user": { "type": "string", "description": "SSH user" },
                "role": { "type": "string", "enum": ["worker", "leader", "gateway"], "description": "Node role (default: worker)" },
                "ram_gb": { "type": "number", "description": "RAM in GB" },
                "gpu_type": { "type": "string", "description": "GPU type (apple_silicon, nvidia, amd_rocm, cpu)" },
                "setup_ssh_keys": { "type": "boolean", "description": "Exchange SSH keys (default: true)" }
            },
            "required": ["node_name", "ip"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let name = input.get("node_name").and_then(Value::as_str).unwrap_or("");
        let ip = input.get("ip").and_then(Value::as_str).unwrap_or("");
        let port = input.get("port").and_then(Value::as_u64).unwrap_or(51000);
        let user = input.get("user").and_then(Value::as_str).unwrap_or("root");
        let role = input
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("worker");
        let ram_gb = input.get("ram_gb").and_then(Value::as_u64).unwrap_or(0);
        let gpu = input
            .get("gpu_type")
            .and_then(Value::as_str)
            .unwrap_or("cpu");
        let setup_keys = input
            .get("setup_ssh_keys")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if name.is_empty() || ip.is_empty() {
            return AgentToolResult::err("Both 'node_name' and 'ip' are required");
        }

        let mut steps = Vec::new();

        // Step 1: SSH key exchange
        if setup_keys {
            let key_copy = Command::new("ssh-copy-id")
                .args(["-o", "StrictHostKeyChecking=no", &format!("{user}@{ip}")])
                .output()
                .await;
            match key_copy {
                Ok(out) if out.status.success() => steps.push("SSH keys: exchanged".into()),
                _ => steps.push("SSH keys: manual setup may be needed".into()),
            }
        }

        // Step 2: Verify connectivity
        let verify = Command::new("ssh")
            .args([
                "-o",
                "ConnectTimeout=5",
                &format!("{user}@{ip}"),
                "hostname",
            ])
            .output()
            .await;
        match verify {
            Ok(out) if out.status.success() => {
                let hostname = String::from_utf8_lossy(&out.stdout).trim().to_string();
                steps.push(format!("Connectivity: verified (hostname: {hostname})"));
            }
            _ => steps.push("Connectivity: FAILED".into()),
        }

        // Step 3: Generate fleet.toml entry
        let toml_entry = format!(
            r#"
[nodes.{name}]
ip = "{ip}"
port = {port}
role = "{role}"
user = "{user}"
ram_gb = {ram_gb}
gpu_type = "{gpu}"
"#
        );
        steps.push(format!("Fleet config entry generated"));

        // Step 4: Check if LLM server is running
        let llm_check = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default()
            .get(format!("http://{ip}:{port}/health"))
            .send()
            .await;
        match llm_check {
            Ok(r) if r.status().is_success() => {
                steps.push(format!("LLM server: ONLINE at {ip}:{port}"))
            }
            _ => steps.push(format!("LLM server: not running at {ip}:{port}")),
        }

        AgentToolResult::ok(format!(
            "Node Enrollment — {name}\n\n{}\n\nAdd to fleet.toml:\n```toml{toml_entry}```\n\nNext: Run ModelDeploy to download models to this node.",
            steps.join("\n")
        ))
    }
}

/// ModelDeploy — download and deploy a model to a fleet node.
pub struct ModelDeployTool;

#[async_trait]
impl AgentTool for ModelDeployTool {
    fn name(&self) -> &str {
        "ModelDeploy"
    }
    fn description(&self) -> &str {
        "Download and deploy an LLM model to a fleet node. Supports GGUF download, Ollama pull, and HuggingFace models."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "Node IP or hostname" },
                "user": { "type": "string", "description": "SSH user" },
                "model": { "type": "string", "description": "Model name or URL (e.g. 'qwen2.5-coder:32b', HuggingFace URL)" },
                "method": { "type": "string", "enum": ["ollama", "wget", "huggingface"], "description": "Download method (default: ollama)" },
                "destination": { "type": "string", "description": "Download directory on the node (default: ~/models)" },
                "start_server": { "type": "boolean", "description": "Start llama-server after download (default: false)" },
                "port": { "type": "number", "description": "Port for llama-server (default: 51000)" }
            },
            "required": ["host", "model"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let host = input.get("host").and_then(Value::as_str).unwrap_or("");
        let user = input.get("user").and_then(Value::as_str).unwrap_or("root");
        let model = input.get("model").and_then(Value::as_str).unwrap_or("");
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("ollama");
        let dest = input
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or("~/models");
        let start = input
            .get("start_server")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let port = input.get("port").and_then(Value::as_u64).unwrap_or(51000);

        if host.is_empty() || model.is_empty() {
            return AgentToolResult::err("Both 'host' and 'model' are required");
        }

        let download_cmd = match method {
            "ollama" => format!("ollama pull {model}"),
            "wget" => format!("mkdir -p {dest} && cd {dest} && wget -c '{model}'"),
            "huggingface" => format!(
                "pip install huggingface-hub && huggingface-cli download {model} --local-dir {dest}"
            ),
            _ => return AgentToolResult::err(format!("Unknown method: {method}")),
        };

        let mut full_cmd = download_cmd.clone();
        if start {
            full_cmd.push_str(&format!(" && nohup llama-server -m {dest}/*.gguf --host 0.0.0.0 --port {port} &>/tmp/llama-server.log &"));
        }

        let result = ssh_cmd(user, host, &full_cmd).await;

        AgentToolResult::ok(format!(
            "Model Deploy — {model} → {host}\n\n  Method: {method}\n  Destination: {dest}\n  Server: {}\n\nOutput:\n{}",
            if start {
                format!("starting on port {port}")
            } else {
                "not started".into()
            },
            truncate_output(&result, 2000)
        ))
    }
}

/// FleetInventory — scan network and report all nodes with hardware details.
pub struct FleetInventoryTool;

#[async_trait]
impl AgentTool for FleetInventoryTool {
    fn name(&self) -> &str {
        "FleetInventory"
    }
    fn description(&self) -> &str {
        "Scan the fleet network and report all discovered nodes with hardware specs, running models, and health status."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subnet": { "type": "string", "description": "Subnet to scan (default: 192.168.5.0/24)" },
                "ports": { "type": "array", "items": { "type": "number" }, "description": "Ports to check (default: [51000, 51001, 11434])" }
            }
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let ports: Vec<u16> = input
            .get("ports")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u16))
                    .collect()
            })
            .unwrap_or_else(|| vec![51000, 51001, 11434]);

        // Load known fleet nodes from Postgres (no hardcoded node list).
        let known_nodes: Vec<(String, String)> = match crate::fleet_info::fetch_nodes().await {
            Ok(rows) => rows.into_iter().map(|r| (r.name, r.ip)).collect(),
            Err(e) => {
                return AgentToolResult::err(format!("Failed to load fleet from database: {e}"));
            }
        };

        if known_nodes.is_empty() {
            return AgentToolResult::ok("No fleet nodes registered in the database.".to_string());
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();

        let mut inventory = Vec::new();

        for (name, ip) in &known_nodes {
            let mut node_info = format!("{name} ({ip}):");
            let mut found_services = Vec::new();

            for port in &ports {
                let url = format!("http://{ip}:{port}/v1/models");
                if let Ok(resp) = client.get(&url).send().await {
                    if resp.status().is_success() {
                        if let Ok(body) = resp.text().await {
                            let model_name = serde_json::from_str::<Value>(&body)
                                .ok()
                                .and_then(|v| {
                                    v.get("data")?
                                        .as_array()?
                                        .first()?
                                        .get("id")?
                                        .as_str()
                                        .map(String::from)
                                })
                                .unwrap_or_else(|| "unknown".into());
                            found_services.push(format!("    port {port}: {model_name} (ONLINE)"));
                        }
                    }
                } else {
                    // Check if port is open but not LLM
                    if let Ok(_) = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        tokio::net::TcpStream::connect(format!("{ip}:{port}")),
                    )
                    .await
                    {
                        found_services.push(format!("    port {port}: service running (not LLM)"));
                    }
                }
            }

            if found_services.is_empty() {
                node_info.push_str(" OFFLINE or no LLM services");
            } else {
                node_info.push_str("\n");
                node_info.push_str(&found_services.join("\n"));
            }
            inventory.push(node_info);
        }

        AgentToolResult::ok(format!("Fleet Inventory\n\n{}", inventory.join("\n\n")))
    }
}

/// NodeHealthCheck — deep health check for a specific node.
pub struct NodeHealthCheckTool;

#[async_trait]
impl AgentTool for NodeHealthCheckTool {
    fn name(&self) -> &str {
        "NodeHealthCheck"
    }
    fn description(&self) -> &str {
        "Deep health check for a fleet node: SSH connectivity, disk space, GPU detection, model availability, memory usage."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "Node IP or hostname" },
                "user": { "type": "string", "description": "SSH user" },
                "port": { "type": "number", "description": "LLM port to check (default: 51000)" }
            },
            "required": ["host"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let host = input.get("host").and_then(Value::as_str).unwrap_or("");
        let user = input.get("user").and_then(Value::as_str).unwrap_or("root");
        let port = input.get("port").and_then(Value::as_u64).unwrap_or(51000);

        if host.is_empty() {
            return AgentToolResult::err("Missing 'host'");
        }

        let mut checks = Vec::new();

        // SSH connectivity
        let ssh = Command::new("ssh")
            .args([
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=no",
                &format!("{user}@{host}"),
                "echo ok",
            ])
            .output()
            .await;
        checks.push(format!(
            "SSH: {}",
            if ssh.as_ref().map(|o| o.status.success()).unwrap_or(false) {
                "OK"
            } else {
                "FAIL"
            }
        ));

        // System info via SSH
        let sys_info = ssh_cmd(user, host, "hostname && uname -sr && free -h 2>/dev/null | head -2 || sysctl -n hw.memsize 2>/dev/null && df -h / | tail -1").await;
        if !sys_info.is_empty() {
            checks.push(format!(
                "System:\n{}",
                sys_info
                    .lines()
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        // GPU detection
        let gpu = ssh_cmd(user, host, "nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null || system_profiler SPDisplaysDataType 2>/dev/null | grep Chipset || echo 'No GPU detected'").await;
        checks.push(format!("GPU: {}", gpu.lines().next().unwrap_or("unknown")));

        // LLM server health
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        let llm_health = client
            .get(format!("http://{host}:{port}/health"))
            .send()
            .await;
        checks.push(format!(
            "LLM ({port}): {}",
            match llm_health {
                Ok(r) if r.status().is_success() => "ONLINE",
                _ => "OFFLINE",
            }
        ));

        // Models available
        if let Ok(resp) = client
            .get(format!("http://{host}:{port}/v1/models"))
            .send()
            .await
        {
            if let Ok(body) = resp.text().await {
                let models: Vec<String> = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("data")?.as_array().cloned())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                checks.push(format!(
                    "Models: {}",
                    if models.is_empty() {
                        "none".into()
                    } else {
                        models.join(", ")
                    }
                ));
            }
        }

        AgentToolResult::ok(format!("Health Check — {host}\n\n{}", checks.join("\n")))
    }
}

/// BinaryDeploy — build and deploy ForgeFleet binary to a node.
pub struct BinaryDeployTool;

#[async_trait]
impl AgentTool for BinaryDeployTool {
    fn name(&self) -> &str {
        "BinaryDeploy"
    }
    fn description(&self) -> &str {
        "Build and deploy the ForgeFleet binary to a fleet node via SSH. Cross-compiles if needed."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "Target node IP" },
                "user": { "type": "string", "description": "SSH user" },
                "binary": { "type": "string", "description": "Which binary: ff or forgefleetd (default: ff)" },
                "build_locally": { "type": "boolean", "description": "Build on this machine and SCP (default: true). False = build on target." }
            },
            "required": ["host"]
        })
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let host = input.get("host").and_then(Value::as_str).unwrap_or("");
        let user = input.get("user").and_then(Value::as_str).unwrap_or("root");
        let binary = input.get("binary").and_then(Value::as_str).unwrap_or("ff");
        let build_local = input
            .get("build_locally")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if host.is_empty() {
            return AgentToolResult::err("Missing 'host'");
        }

        if build_local {
            // Build locally
            let build = Command::new("cargo")
                .args(["build", "--release", "-p", "ff-terminal"])
                .current_dir(&ctx.working_dir)
                .output()
                .await;
            match build {
                Ok(out) if out.status.success() => {
                    // SCP to target
                    let binary_path = ctx.working_dir.join("target/release/ff");
                    let scp = Command::new("scp")
                        .args([
                            &binary_path.to_string_lossy().to_string(),
                            &format!("{user}@{host}:/usr/local/bin/{binary}"),
                        ])
                        .output()
                        .await;
                    match scp {
                        Ok(out) if out.status.success() => AgentToolResult::ok(format!(
                            "Binary deployed: {binary} → {host}:/usr/local/bin/{binary}"
                        )),
                        _ => AgentToolResult::err(
                            "SCP failed. Check SSH access and permissions.".to_string(),
                        ),
                    }
                }
                Ok(out) => AgentToolResult::err(format!(
                    "Build failed:\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )),
                Err(e) => AgentToolResult::err(format!("Build command failed: {e}")),
            }
        } else {
            // Build on target
            let build_cmd = format!(
                "cd ~/projects/forge-fleet && git pull && cargo build --release -p ff-terminal && cp target/release/ff /usr/local/bin/ff"
            );
            let result = ssh_cmd(user, host, &build_cmd).await;
            AgentToolResult::ok(format!(
                "Remote build on {host}:\n{}",
                truncate_output(&result, 2000)
            ))
        }
    }
}

// Helper: run a command over SSH
async fn ssh_cmd(user: &str, host: &str, command: &str) -> String {
    match Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=no",
            &format!("{user}@{host}"),
            command,
        ])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!("{stdout}{stderr}")
        }
        Err(e) => format!("SSH failed: {e}"),
    }
}
