//! Standard ForgeFleet/network port conventions.

/// Default LLM model ports used across the fleet.
/// Convention: each node uses 55000 for first model, 55001 for second, etc.
pub const LLM_MODEL_PORTS: [u16; 11] = [55000, 55001, 55002, 55003, 55004, 55005, 55006, 55007, 55008, 55009, 55010];

/// Legacy llama.cpp ports (kept for backward compatibility in scanning).
pub const LLAMA_CPP_PORTS: [u16; 4] = [55000, 55001, 55002, 55003];

/// Common LLM/API service ports that may exist on worker nodes.
pub const OLLAMA_PORT: u16 = 11434;
pub const OPENAI_COMPAT_PORT: u16 = 8000;
pub const ALT_OPENAI_COMPAT_PORT: u16 = 8080;

/// Fleet control/health conventions.
pub const HEALTH_PATH: &str = "/health";

/// Returns known LLM-serving ports used for model discovery.
pub fn known_llm_ports() -> Vec<u16> {
    let mut ports = LLM_MODEL_PORTS.to_vec();
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
