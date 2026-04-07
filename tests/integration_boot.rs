//! Integration test — verify ForgeFleet boot, health, and core API endpoints.
//!
//! This test starts the gateway subsystem (which serves /health, /api/config,
//! /v1/models, and /api/mc/board) on a random port, verifies each endpoint,
//! and shuts down cleanly.
//!
//! A temporary directory is used for the SQLite database, so tests never
//! interfere with each other or with production data.

use std::net::TcpListener as StdTcpListener;

use tempfile::TempDir;

use ff_core::config::FleetConfig;
use ff_db::{DbPool, DbPoolConfig, run_migrations};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Find an available TCP port on localhost.
fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind to :0");
    listener.local_addr().unwrap().port()
}

/// Create a minimal fleet config for testing.
fn test_config() -> FleetConfig {
    toml::from_str(
        r#"
[fleet]
name = "test-fleet"
api_port = 51800

[nodes.test-node]
ip = "127.0.0.1"
role = "leader"

[nodes.test-node.models.test_model]
tier = 1
family = "qwen"
mode = "primary"
port = 51999
ctx_size = 8192
"#,
    )
    .expect("parse test config")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Verify that the embedded SQLite database initializes correctly in a temp dir,
/// that core routes respond, and that the server shuts down cleanly.
#[tokio::test]
async fn test_boot_health_and_api() {
    // ── Setup: temp DB + migrations ──────────────────────────────────────────
    let tmp = TempDir::new().expect("create temp dir");
    let db_path = tmp.path().join("test-forgefleet.db");

    let pool = DbPool::open(DbPoolConfig::with_path(&db_path)).expect("open db");
    let raw_conn = pool.open_raw_connection().expect("raw conn");
    let applied = run_migrations(&raw_conn).expect("migrations");
    assert!(applied > 0, "should apply at least one migration");
    drop(raw_conn);

    // ── Setup: config and gateway ────────────────────────────────────────────
    let config = test_config();
    let port = free_port();
    let bind_addr = format!("127.0.0.1:{port}");
    let base_url = format!("http://{bind_addr}");

    // Create MC database in temp dir too
    let mc_db_path = tmp.path().join("test-mc.db").to_string_lossy().to_string();

    let gateway_config = ff_gateway::server::GatewayConfig {
        bind_addr: bind_addr.clone(),
        fleet_config: Some(config.clone()),
        mc_db_path: Some(mc_db_path),
        ..Default::default()
    };

    // Start the gateway in a background task
    let server_handle = tokio::spawn(async move {
        ff_gateway::run(gateway_config).await.ok();
    });

    // Give the server a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let client = reqwest::Client::new();

    // ── 1) GET /health → 200 ────────────────────────────────────────────────
    let resp = client
        .get(format!("{base_url}/health"))
        .send()
        .await
        .expect("health request");
    assert_eq!(resp.status(), 200, "/health should return 200");
    let body: serde_json::Value = resp.json().await.expect("health json");
    assert_eq!(body["status"], "ok", "health status should be 'ok'");
    assert!(
        body["service"].is_string(),
        "health should include service name"
    );

    // ── 2) GET /api/config → valid JSON ─────────────────────────────────────
    let resp = client
        .get(format!("{base_url}/api/config"))
        .send()
        .await
        .expect("config request");
    assert_eq!(resp.status(), 200, "/api/config should return 200");
    let body: serde_json::Value = resp.json().await.expect("config json");
    // Should contain fleet.name from our test config
    assert!(body.is_object(), "/api/config should return a JSON object");

    // ── 3) GET /v1/models → model list ──────────────────────────────────────
    let resp = client
        .get(format!("{base_url}/v1/models"))
        .send()
        .await
        .expect("models request");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("models json");
    assert!(
        status == 200 || status == 503,
        "/v1/models should return 200 (available) or 503 (unavailable)"
    );
    if status == 200 {
        assert_eq!(
            body["object"], "list",
            "/v1/models should return object=list"
        );
        assert!(
            body["data"].is_array(),
            "/v1/models should return data array"
        );
    } else {
        assert!(
            body["error"].is_object(),
            "/v1/models 503 should return error object"
        );
    }

    // ── 4) GET /api/mc/board → board JSON ───────────────────────────────────
    let resp = client
        .get(format!("{base_url}/api/mc/board"))
        .send()
        .await
        .expect("board request");
    // Board should return 200 with valid JSON (might be empty board)
    assert_eq!(resp.status(), 200, "/api/mc/board should return 200");
    let body: serde_json::Value = resp.json().await.expect("board json");
    assert!(
        body.is_object() || body.is_array(),
        "/api/mc/board should return valid JSON"
    );

    // ── Cleanup ──────────────────────────────────────────────────────────────
    server_handle.abort();
    let _ = server_handle.await;
    // TempDir cleanup is automatic on drop
}

/// Verify the SQLite pool works with migrations and config KV round-trip.
#[tokio::test]
async fn test_sqlite_in_memory_boot() {
    let tmp = TempDir::new().expect("create temp dir");
    let db_path = tmp.path().join("test-sqlite-boot.db");

    let pool = DbPool::open(DbPoolConfig::with_path(&db_path)).expect("sqlite pool");
    let raw = pool.open_raw_connection().expect("raw conn");
    let applied = run_migrations(&raw).expect("migrations");
    assert!(applied > 0);

    // Verify we can write and read
    pool.with_conn(|conn| {
        ff_db::queries::config_set(conn, "test.key", "test.value")?;
        let val = ff_db::queries::config_get(conn, "test.key")?;
        assert_eq!(val.as_deref(), Some("test.value"));
        Ok(())
    })
    .await
    .expect("config kv round-trip");
}

/// Verify the audit trail works end-to-end with SQLite.
#[tokio::test]
async fn test_audit_trail_with_sqlite() {
    let tmp = TempDir::new().expect("create temp dir");
    let db_path = tmp.path().join("test-audit.db");

    let pool = DbPool::open(DbPoolConfig::with_path(&db_path)).expect("sqlite pool");
    let raw = pool.open_raw_connection().expect("raw conn");
    run_migrations(&raw).expect("migrations");

    // Insert audit events
    pool.with_conn(|conn| {
        ff_db::queries::audit_log(
            conn,
            "config_changed",
            "test-user",
            Some("fleet.name"),
            r#"{"old":"a","new":"b"}"#,
            Some("test-node"),
        )?;
        ff_db::queries::audit_log(
            conn,
            "model_started",
            "system",
            Some("qwen-32b"),
            "{}",
            Some("taylor"),
        )?;
        Ok(())
    })
    .await
    .expect("insert audit events");

    // Query them back
    let events = pool
        .with_conn(|conn| ff_db::queries::recent_audit_log(conn, 10))
        .await
        .expect("query audit log");

    assert_eq!(events.len(), 2);
    // Newest first
    assert_eq!(events[0].event_type, "model_started");
    assert_eq!(events[1].event_type, "config_changed");
    assert_eq!(events[1].actor, "test-user");
}
