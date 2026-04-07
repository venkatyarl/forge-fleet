//! Standard ForgeFleet/network port conventions.

/// Default llama.cpp/OpenAI-compatible ports used across the fleet.
pub const LLAMA_CPP_PORTS: [u16; 4] = [51800, 51801, 51802, 51803];

/// Common LLM/API service ports that may exist on worker nodes.
pub const OLLAMA_PORT: u16 = 11434;
pub const OPENAI_COMPAT_PORT: u16 = 8000;
pub const ALT_OPENAI_COMPAT_PORT: u16 = 8080;

/// Fleet control/health conventions.
pub const HEALTH_PATH: &str = "/health";

/// Returns known LLM-serving ports used for model discovery.
pub fn known_llm_ports() -> Vec<u16> {
    let mut ports = LLAMA_CPP_PORTS.to_vec();
    ports.extend([OLLAMA_PORT, OPENAI_COMPAT_PORT, ALT_OPENAI_COMPAT_PORT]);
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Returns all service ports worth probing during subnet discovery.
pub fn known_service_ports() -> Vec<u16> {
    known_llm_ports()
}

/// Fast check for whether a port is one of the known LLM ports.
pub fn is_llm_port(port: u16) -> bool {
    known_llm_ports().contains(&port)
}
