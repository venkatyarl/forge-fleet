//! ForgeFleet hardware and node discovery.

pub mod activity;
pub mod hardware;
pub mod health;
pub mod models;
pub mod ports;
pub mod profile;
pub mod registry;
pub mod scanner;

pub use activity::{ActivitySignals, read_activity_signals};
pub use hardware::{
    CpuProfile, GpuType, HardwareProfile, InterconnectType, MemoryProfile, MemoryType,
    detect_hardware_profile,
};
pub use health::{
    HealthCheckResult, HealthMonitor, HealthSnapshot, HealthStatus, HealthTarget,
    collect_health_snapshot,
};
pub use models::{
    EndpointModelInfo, ModelCard, ModelListResponse, query_models_endpoint, query_models_endpoints,
};
pub use ports::{known_llm_ports, known_service_ports};
pub use registry::{FleetNode, NodeRegistry};
pub use scanner::{
    DiscoveredNode, DiscoveryError, NodeScanResult, NodeScanStatus, NodeScanner, ScanTarget,
    ScannerConfig, build_scan_targets, scan_subnet,
};
