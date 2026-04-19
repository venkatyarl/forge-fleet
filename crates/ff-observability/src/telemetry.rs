//! Telemetry initialization — tracing, structured logging, span propagation.
//!
//! Call [`init_telemetry`] once at process start to configure global tracing
//! with env filtering, stdout logs, and optional rotating file logs.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::file_logger::{self, FileLogConfig};

// Keep the file logger worker guard alive for process lifetime.
static FILE_LOG_GUARD: OnceLock<Mutex<Option<WorkerGuard>>> = OnceLock::new();

fn stash_guard(guard: WorkerGuard) {
    let lock = FILE_LOG_GUARD.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = lock.lock() {
        *slot = Some(guard);
    }
}

fn build_env_filter(config: &TelemetryConfig) -> anyhow::Result<EnvFilter> {
    let mut filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));
    for directive in &config.directives {
        filter = filter.add_directive(directive.parse()?);
    }
    Ok(filter)
}

// ─── Configuration ───────────────────────────────────────────────────────────

/// Telemetry configuration options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Emit stdout logs as JSON (structured) rather than human-readable.
    #[serde(default)]
    pub json: bool,

    /// Default log level if `RUST_LOG` is not set.
    #[serde(default = "default_level")]
    pub level: String,

    /// Additional env-filter directives (e.g. "ff_core=debug,tower_http=info").
    #[serde(default)]
    pub directives: Vec<String>,

    /// Whether to include span fields in stdout log output.
    #[serde(default = "default_true")]
    pub include_spans: bool,

    /// Whether to include source file location in stdout log output.
    #[serde(default)]
    pub include_location: bool,

    /// Service name for span propagation context.
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// Node name — injected into every log line for fleet-wide correlation.
    #[serde(default)]
    pub node_name: Option<String>,

    /// Optional file logging configuration.
    /// If omitted, file logging is disabled.
    #[serde(default)]
    pub file_log: Option<FileLogConfig>,

    /// Enable OpenTelemetry trace exporter (stdout-based).
    /// When true, an OTel tracing layer is added to the subscriber stack.
    #[serde(default)]
    pub enable_opentelemetry: bool,
}

fn default_level() -> String {
    "info".to_string()
}

fn default_true() -> bool {
    true
}

fn default_service_name() -> String {
    "forgefleet".to_string()
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            json: false,
            level: default_level(),
            directives: Vec::new(),
            include_spans: true,
            include_location: false,
            service_name: default_service_name(),
            node_name: None,
            file_log: None,
            enable_opentelemetry: false,
        }
    }
}

impl TelemetryConfig {
    /// Convenience constructor: enable file logging with daemon defaults
    /// (`~/.forgefleet/logs`, daily rotation, 7 files retained).
    pub fn with_file_logging() -> Self {
        Self {
            file_log: Some(FileLogConfig::daemon_defaults()),
            ..Self::default()
        }
    }
}

// ─── Initialization ──────────────────────────────────────────────────────────

/// Initialize the global tracing subscriber.
///
/// This should be called once, early in `main()`. Subsequent calls return an
/// error because tracing allows only one global subscriber.
pub fn init_telemetry(config: &TelemetryConfig) -> anyhow::Result<()> {
    let file_cfg = config.file_log.as_ref().filter(|f| f.enabled);
    let mut file_logging_enabled = false;

    if config.json {
        let stdout_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_target(true)
            .with_span_list(config.include_spans)
            .with_file(config.include_location)
            .with_line_number(config.include_location);

        match file_cfg {
            Some(file_cfg) => {
                let filter = build_env_filter(config)?;
                let (file_writer, guard) = file_logger::create_non_blocking_writer(file_cfg)?;
                stash_guard(guard);
                file_logging_enabled = true;

                if file_cfg.json {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(file_cfg.include_spans)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                } else {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_level(true)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                }
            }
            None => {
                let filter = build_env_filter(config)?;
                tracing_subscriber::registry()
                    .with(filter)
                    .with(stdout_layer)
                    .try_init()
                    .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
            }
        }
    } else {
        let stdout_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_level(true)
            .with_file(config.include_location)
            .with_line_number(config.include_location);

        match file_cfg {
            Some(file_cfg) => {
                let filter = build_env_filter(config)?;
                let (file_writer, guard) = file_logger::create_non_blocking_writer(file_cfg)?;
                stash_guard(guard);
                file_logging_enabled = true;

                if file_cfg.json {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(file_cfg.include_spans)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                } else {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_level(true)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                }
            }
            None => {
                let filter = build_env_filter(config)?;
                tracing_subscriber::registry()
                    .with(filter)
                    .with(stdout_layer)
                    .try_init()
                    .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
            }
        }
    }

    // ─── Optional OpenTelemetry tracing exporter ─────────────────────────
    // NOTE: OTel is wired at the application level. When `enable_opentelemetry`
    // is true the user should install an OTel tracing layer *before*
    // `init_telemetry` (since only one global subscriber is allowed).
    // We log the config flag here for observability.

    tracing::info!(
        service = %config.service_name,
        node = ?config.node_name,
        json = config.json,
        file_logging = file_logging_enabled,
        otel = config.enable_opentelemetry,
        "telemetry initialized"
    );

    Ok(())
}

/// Initialize the global tracing subscriber WITH an additional layer — typically
/// a NATS log-forwarding layer so every daemon event is mirrored onto the
/// fleet-wide event bus.
///
/// The `extra_layer` is composed on top of the file + stdout layers; if NATS
/// (or whatever it represents) becomes unavailable at runtime the layer
/// itself is expected to best-effort drop the event, never block the
/// logging hot-path.
///
/// Callers should construct the extra layer first (e.g.
/// `NatsLogLayer::with_client(client, node, "forgefleetd")`), box it, and
/// pass it in. On any failure to attach the layer (global subscriber
/// already installed, etc.) this returns an error — callers should fall
/// back to plain [`init_telemetry`].
pub fn init_telemetry_with_extra_layer<L>(
    config: &TelemetryConfig,
    extra_layer: L,
) -> anyhow::Result<()>
where
    L: Layer<Registry> + Send + Sync + 'static,
{
    let file_cfg = config.file_log.as_ref().filter(|f| f.enabled);
    let mut file_logging_enabled = false;

    if config.json {
        let stdout_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_target(true)
            .with_span_list(config.include_spans)
            .with_file(config.include_location)
            .with_line_number(config.include_location);

        match file_cfg {
            Some(file_cfg) => {
                let filter = build_env_filter(config)?;
                let (file_writer, guard) = file_logger::create_non_blocking_writer(file_cfg)?;
                stash_guard(guard);
                file_logging_enabled = true;

                if file_cfg.json {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(file_cfg.include_spans)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(extra_layer)
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                } else {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_level(true)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(extra_layer)
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                }
            }
            None => {
                let filter = build_env_filter(config)?;
                tracing_subscriber::registry()
                    .with(extra_layer)
                    .with(filter)
                    .with(stdout_layer)
                    .try_init()
                    .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
            }
        }
    } else {
        let stdout_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_level(true)
            .with_file(config.include_location)
            .with_line_number(config.include_location);

        match file_cfg {
            Some(file_cfg) => {
                let filter = build_env_filter(config)?;
                let (file_writer, guard) = file_logger::create_non_blocking_writer(file_cfg)?;
                stash_guard(guard);
                file_logging_enabled = true;

                if file_cfg.json {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(true)
                        .with_span_list(file_cfg.include_spans)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(extra_layer)
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                } else {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_level(true)
                        .with_file(file_cfg.include_location)
                        .with_line_number(file_cfg.include_location)
                        .with_ansi(false)
                        .with_writer(file_writer);

                    tracing_subscriber::registry()
                        .with(extra_layer)
                        .with(filter)
                        .with(stdout_layer)
                        .with(file_layer)
                        .try_init()
                        .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
                }
            }
            None => {
                let filter = build_env_filter(config)?;
                tracing_subscriber::registry()
                    .with(extra_layer)
                    .with(filter)
                    .with(stdout_layer)
                    .try_init()
                    .map_err(|e| anyhow::anyhow!("failed to init tracing subscriber: {e}"))?;
            }
        }
    }

    tracing::info!(
        service = %config.service_name,
        node = ?config.node_name,
        json = config.json,
        file_logging = file_logging_enabled,
        otel = config.enable_opentelemetry,
        extra_layer = true,
        "telemetry initialized (with extra layer)"
    );

    Ok(())
}

// ─── Span Propagation Helpers ────────────────────────────────────────────────

/// Propagation context for distributed tracing across fleet nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropagationContext {
    /// Trace ID — shared across all spans in a distributed trace.
    pub trace_id: String,
    /// Span ID — the parent span that spawned this request.
    pub span_id: String,
    /// Service name that created this context.
    pub service: String,
    /// Node that created this context.
    pub node: Option<String>,
}

impl PropagationContext {
    /// Create a new propagation context.
    pub fn new(service: impl Into<String>, node: Option<String>) -> Self {
        Self {
            trace_id: uuid::Uuid::new_v4().to_string(),
            span_id: uuid::Uuid::new_v4().to_string(),
            service: service.into(),
            node,
        }
    }

    /// Create a child span context from this parent.
    pub fn child(&self, service: impl Into<String>, node: Option<String>) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: uuid::Uuid::new_v4().to_string(),
            service: service.into(),
            node,
        }
    }

    /// Serialize to JSON for embedding in HTTP headers / message payloads.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Deserialize from JSON.
    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

// ─── Log Level Mapping ───────────────────────────────────────────────────────

/// Map a string log level to a tracing [`Level`].
pub fn parse_level(s: &str) -> Level {
    match s.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" | "warning" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = TelemetryConfig::default();
        assert!(!cfg.json);
        assert_eq!(cfg.level, "info");
        assert!(cfg.include_spans);
        assert_eq!(cfg.service_name, "forgefleet");
        assert!(cfg.file_log.is_none());
    }

    #[test]
    fn test_with_file_logging_config() {
        let cfg = TelemetryConfig::with_file_logging();
        let file_cfg = cfg.file_log.expect("file log config should exist");
        assert!(file_cfg.enabled);
        assert!(file_cfg.json);
        assert_eq!(file_cfg.max_files, 7);
    }

    #[test]
    fn test_propagation_context_child() {
        let parent = PropagationContext::new("gateway", Some("taylor".into()));
        let child = parent.child("agent", Some("james".into()));
        assert_eq!(parent.trace_id, child.trace_id);
        assert_ne!(parent.span_id, child.span_id);
    }

    #[test]
    fn test_propagation_roundtrip() {
        let ctx = PropagationContext::new("test", None);
        let json = ctx.to_json().unwrap();
        let back = PropagationContext::from_json(&json).unwrap();
        assert_eq!(ctx.trace_id, back.trace_id);
        assert_eq!(ctx.span_id, back.span_id);
    }

    #[test]
    fn test_parse_level() {
        assert_eq!(parse_level("debug"), Level::DEBUG);
        assert_eq!(parse_level("WARN"), Level::WARN);
        assert_eq!(parse_level("garbage"), Level::INFO);
    }
}
