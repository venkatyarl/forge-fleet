//! etcd v3 client for HA cluster discovery.
//!
//! Talks to etcd's gRPC-JSON gateway (`POST /v3/...`) over plain HTTP, so no
//! gRPC/tonic dependency is needed. Per the gateway contract, keys and values
//! travel base64-encoded and int64 fields are JSON strings.
//!
//! `EtcdClient::connect` probes the configured endpoints and picks the first
//! healthy one; unary calls fail over to the remaining endpoints on transport
//! errors. `watch_prefix` opens a long-lived `/v3/watch` stream and forwards
//! key events over a channel.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;

#[derive(Debug, Error)]
pub enum EtcdError {
    #[error("no etcd endpoints configured")]
    NoEndpoints,
    #[error("all etcd endpoints failed; last error: {last_error}")]
    AllEndpointsFailed { last_error: String },
    #[error("etcd request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("etcd returned HTTP {status} from {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("invalid etcd payload: {0}")]
    InvalidPayload(String),
}

#[derive(Debug, Clone)]
pub struct EtcdConfig {
    /// etcd endpoints, e.g. `["http://10.0.0.1:2379", "10.0.0.2:2379"]`.
    /// A missing scheme defaults to `http://`.
    pub endpoints: Vec<String>,
    pub connect_timeout: Duration,
    /// Timeout for unary requests (not applied to watch streams).
    pub request_timeout: Duration,
}

impl Default for EtcdConfig {
    fn default() -> Self {
        Self {
            endpoints: vec!["http://127.0.0.1:2379".to_string()],
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(5),
        }
    }
}

/// A decoded etcd key-value pair.
#[derive(Debug, Clone, Serialize)]
pub struct EtcdKeyValue {
    pub key: String,
    pub value: String,
    pub create_revision: i64,
    pub mod_revision: i64,
    pub version: i64,
    pub lease: i64,
}

/// Cluster status of the endpoint the client is connected to.
#[derive(Debug, Clone, Serialize)]
pub struct EtcdStatus {
    pub endpoint: String,
    pub version: String,
    pub leader: String,
    pub revision: i64,
    pub raft_term: i64,
}

/// A single change observed by a watch.
#[derive(Debug, Clone)]
pub enum EtcdWatchEvent {
    Put(EtcdKeyValue),
    Delete(EtcdKeyValue),
}

/// Handle to a running watch stream. Dropping it cancels the stream.
pub struct EtcdWatcher {
    events: mpsc::Receiver<EtcdWatchEvent>,
    task: JoinHandle<()>,
}

impl EtcdWatcher {
    /// Receive the next event; `None` means the stream ended (caller should
    /// re-establish the watch from its last known revision).
    pub async fn recv(&mut self) -> Option<EtcdWatchEvent> {
        self.events.recv().await
    }
}

impl Drop for EtcdWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone)]
pub struct EtcdClient {
    /// All endpoints, active one first.
    endpoints: Vec<String>,
    http: reqwest::Client,
    request_timeout: Duration,
}

impl EtcdClient {
    /// Probe endpoints in order and connect to the first healthy one.
    pub async fn connect(config: EtcdConfig) -> Result<Self, EtcdError> {
        if config.endpoints.is_empty() {
            return Err(EtcdError::NoEndpoints);
        }
        let endpoints: Vec<String> = config
            .endpoints
            .iter()
            .map(|e| normalize_endpoint(e))
            .collect();
        // No global timeout: the same client serves long-lived watch streams.
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .build()?;

        let mut last_error = String::from("unreachable");
        for (i, endpoint) in endpoints.iter().enumerate() {
            let client = Self {
                endpoints: rotate_front(&endpoints, i),
                http: http.clone(),
                request_timeout: config.request_timeout,
            };
            match client.status().await {
                Ok(status) => {
                    tracing::info!(
                        endpoint = %endpoint,
                        version = %status.version,
                        "connected to etcd"
                    );
                    return Ok(client);
                }
                Err(err) => {
                    tracing::warn!(endpoint = %endpoint, error = %err, "etcd endpoint probe failed");
                    last_error = err.to_string();
                }
            }
        }
        Err(EtcdError::AllEndpointsFailed { last_error })
    }

    /// The endpoint currently used for requests.
    pub fn active_endpoint(&self) -> &str {
        &self.endpoints[0]
    }

    /// Fetch cluster status from the active endpoint.
    pub async fn status(&self) -> Result<EtcdStatus, EtcdError> {
        let raw: WireStatusResponse = self
            .post_json("/v3/maintenance/status", &serde_json::json!({}))
            .await?;
        Ok(EtcdStatus {
            endpoint: self.active_endpoint().to_string(),
            version: raw.version,
            leader: raw.leader,
            revision: raw.header.revision,
            raft_term: raw.header.raft_term,
        })
    }

    /// Write a key.
    pub async fn put(&self, key: &str, value: &str) -> Result<(), EtcdError> {
        let body = serde_json::json!({
            "key": BASE64.encode(key),
            "value": BASE64.encode(value),
        });
        let _: serde_json::Value = self.post_json("/v3/kv/put", &body).await?;
        Ok(())
    }

    /// Read a single key.
    pub async fn get(&self, key: &str) -> Result<Option<EtcdKeyValue>, EtcdError> {
        let body = serde_json::json!({ "key": BASE64.encode(key) });
        let raw: WireRangeResponse = self.post_json("/v3/kv/range", &body).await?;
        let mut kvs = decode_kvs(raw.kvs)?;
        Ok(kvs.drain(..).next())
    }

    /// Read all keys under a prefix.
    pub async fn get_prefix(&self, prefix: &str) -> Result<Vec<EtcdKeyValue>, EtcdError> {
        let body = serde_json::json!({
            "key": BASE64.encode(prefix),
            "range_end": BASE64.encode(prefix_range_end(prefix.as_bytes())),
        });
        let raw: WireRangeResponse = self.post_json("/v3/kv/range", &body).await?;
        decode_kvs(raw.kvs)
    }

    /// Watch all keys under a prefix, optionally starting at a revision.
    ///
    /// Connection errors surface here; once established, events stream through
    /// the returned watcher until the connection drops or it is dropped.
    pub async fn watch_prefix(
        &self,
        prefix: &str,
        start_revision: Option<i64>,
    ) -> Result<EtcdWatcher, EtcdError> {
        let mut create_request = serde_json::json!({
            "key": BASE64.encode(prefix),
            "range_end": BASE64.encode(prefix_range_end(prefix.as_bytes())),
        });
        if let Some(rev) = start_revision {
            create_request["start_revision"] = serde_json::json!(rev.to_string());
        }
        let body = serde_json::json!({ "create_request": create_request });

        let url = format!("{}/v3/watch", self.active_endpoint());
        let mut response = self.http.post(&url).json(&body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(EtcdError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }

        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(bytes)) => {
                        buf.extend_from_slice(&bytes);
                        for line in drain_complete_lines(&mut buf) {
                            match parse_watch_line(&line) {
                                Ok(events) => {
                                    for event in events {
                                        if tx.send(event).await.is_err() {
                                            return; // watcher dropped
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!(error = %err, "bad etcd watch frame");
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("etcd watch stream closed by server");
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "etcd watch stream error");
                        return;
                    }
                }
            }
        });

        Ok(EtcdWatcher { events: rx, task })
    }

    /// POST a JSON body, failing over across endpoints on transport errors.
    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, EtcdError> {
        let mut last_error: Option<EtcdError> = None;
        for endpoint in &self.endpoints {
            let url = format!("{endpoint}{path}");
            let result = self
                .http
                .post(&url)
                .timeout(self.request_timeout)
                .json(body)
                .send()
                .await;
            match result {
                Ok(response) => {
                    let status = response.status();
                    if !status.is_success() {
                        // The server answered: report it rather than failing over.
                        let body = response.text().await.unwrap_or_default();
                        return Err(EtcdError::Status {
                            status: status.as_u16(),
                            url,
                            body,
                        });
                    }
                    return response.json::<T>().await.map_err(EtcdError::Http);
                }
                Err(err) => last_error = Some(EtcdError::Http(err)),
            }
        }
        Err(last_error.unwrap_or(EtcdError::NoEndpoints))
    }
}

// ─── Wire types (gRPC-JSON gateway) ──────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct WireHeader {
    #[serde(default, deserialize_with = "de_i64")]
    revision: i64,
    #[serde(default, deserialize_with = "de_i64")]
    raft_term: i64,
}

#[derive(Debug, Deserialize)]
struct WireStatusResponse {
    #[serde(default)]
    header: WireHeader,
    #[serde(default)]
    version: String,
    #[serde(default)]
    leader: String,
}

#[derive(Debug, Deserialize)]
struct WireRangeResponse {
    #[serde(default)]
    kvs: Vec<WireKeyValue>,
}

#[derive(Debug, Deserialize)]
struct WireKeyValue {
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default, deserialize_with = "de_i64")]
    create_revision: i64,
    #[serde(default, deserialize_with = "de_i64")]
    mod_revision: i64,
    #[serde(default, deserialize_with = "de_i64")]
    version: i64,
    #[serde(default, deserialize_with = "de_i64")]
    lease: i64,
}

#[derive(Debug, Deserialize)]
struct WireWatchLine {
    #[serde(default)]
    result: Option<WireWatchResponse>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct WireWatchResponse {
    #[serde(default)]
    canceled: bool,
    #[serde(default)]
    events: Vec<WireEvent>,
}

#[derive(Debug, Deserialize)]
struct WireEvent {
    /// Absent for PUT: the gateway omits default enum values.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    kv: Option<WireKeyValue>,
}

/// int64 fields arrive as JSON strings from the gateway; tolerate numbers too.
fn de_i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(i64),
        Str(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Int(n) => Ok(n),
        Raw::Str(s) => s.parse::<i64>().map_err(serde::de::Error::custom),
    }
}

fn decode_kv(raw: WireKeyValue) -> Result<EtcdKeyValue, EtcdError> {
    let decode = |field: &str, b64: &str| -> Result<String, EtcdError> {
        let bytes = BASE64
            .decode(b64)
            .map_err(|e| EtcdError::InvalidPayload(format!("bad base64 in {field}: {e}")))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    Ok(EtcdKeyValue {
        key: decode("key", &raw.key)?,
        value: decode("value", &raw.value)?,
        create_revision: raw.create_revision,
        mod_revision: raw.mod_revision,
        version: raw.version,
        lease: raw.lease,
    })
}

fn decode_kvs(raw: Vec<WireKeyValue>) -> Result<Vec<EtcdKeyValue>, EtcdError> {
    raw.into_iter().map(decode_kv).collect()
}

/// Parse one newline-delimited watch frame into events.
fn parse_watch_line(line: &str) -> Result<Vec<EtcdWatchEvent>, EtcdError> {
    let frame: WireWatchLine = serde_json::from_str(line)
        .map_err(|e| EtcdError::InvalidPayload(format!("bad watch frame: {e}")))?;
    if let Some(error) = frame.error {
        return Err(EtcdError::InvalidPayload(format!("watch error: {error}")));
    }
    let Some(result) = frame.result else {
        return Ok(vec![]);
    };
    if result.canceled {
        return Ok(vec![]);
    }
    let mut events = Vec::with_capacity(result.events.len());
    for event in result.events {
        let Some(kv) = event.kv else { continue };
        let kv = decode_kv(kv)?;
        match event.kind.as_deref() {
            Some("DELETE") => events.push(EtcdWatchEvent::Delete(kv)),
            // The gateway omits the type for PUT (proto default).
            Some("PUT") | None => events.push(EtcdWatchEvent::Put(kv)),
            Some(other) => {
                return Err(EtcdError::InvalidPayload(format!(
                    "unknown watch event type: {other}"
                )));
            }
        }
    }
    Ok(events)
}

/// Extract complete `\n`-terminated lines from the stream buffer, leaving any
/// trailing partial frame in place.
fn drain_complete_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
        let text = text.trim();
        if !text.is_empty() {
            lines.push(text.to_string());
        }
    }
    lines
}

/// Compute the etcd `range_end` covering all keys with the given prefix:
/// increment the last non-0xff byte and truncate. An empty (or all-0xff)
/// prefix yields `[0]`, which etcd interprets as "the whole keyspace".
fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(&last) = end.last() {
        if last < 0xff {
            *end.last_mut().unwrap() += 1;
            return end;
        }
        end.pop();
    }
    vec![0]
}

fn normalize_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Endpoints with index `i` rotated to the front (active endpoint first).
fn rotate_front(endpoints: &[String], i: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(endpoints.len());
    out.extend_from_slice(&endpoints[i..]);
    out.extend_from_slice(&endpoints[..i]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_range_end() {
        assert_eq!(prefix_range_end(b"/fleet/nodes/"), b"/fleet/nodes0");
        assert_eq!(prefix_range_end(b"a"), b"b");
        assert_eq!(prefix_range_end(b""), vec![0]);
        assert_eq!(prefix_range_end(&[0xff, 0xff]), vec![0]);
        assert_eq!(prefix_range_end(&[b'a', 0xff]), vec![b'b']);
    }

    #[test]
    fn test_normalize_endpoint() {
        assert_eq!(normalize_endpoint("10.0.0.1:2379"), "http://10.0.0.1:2379");
        assert_eq!(
            normalize_endpoint("http://10.0.0.1:2379/"),
            "http://10.0.0.1:2379"
        );
        assert_eq!(normalize_endpoint("https://etcd:2379"), "https://etcd:2379");
    }

    #[test]
    fn test_rotate_front() {
        let eps: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(rotate_front(&eps, 0), eps);
        assert_eq!(rotate_front(&eps, 1), vec!["b", "c", "a"]);
    }

    #[test]
    fn test_drain_complete_lines_across_chunks() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"{\"a\":1}\n{\"b\":");
        assert_eq!(drain_complete_lines(&mut buf), vec!["{\"a\":1}"]);
        buf.extend_from_slice(b"2}\n");
        assert_eq!(drain_complete_lines(&mut buf), vec!["{\"b\":2}"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_parse_watch_line_put_without_type() {
        // PUT events omit "type" (proto default) and int64s are strings.
        let line = r#"{"result":{"header":{"revision":"7"},"events":[
            {"kv":{"key":"L2ZsZWV0L25vZGVzL2E=","value":"eyJpcCI6IjEwLjAuMC4xIn0=",
                   "create_revision":"5","mod_revision":"7","version":"2"}}]}}"#
            .replace('\n', "");
        let events = parse_watch_line(&line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            EtcdWatchEvent::Put(kv) => {
                assert_eq!(kv.key, "/fleet/nodes/a");
                assert_eq!(kv.value, r#"{"ip":"10.0.0.1"}"#);
                assert_eq!(kv.mod_revision, 7);
            }
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_watch_line_delete() {
        let line = r#"{"result":{"events":[{"type":"DELETE","kv":{"key":"L2ZsZWV0L25vZGVzL2E=","mod_revision":"9"}}]}}"#;
        let events = parse_watch_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            EtcdWatchEvent::Delete(kv) => {
                assert_eq!(kv.key, "/fleet/nodes/a");
                assert_eq!(kv.value, "");
                assert_eq!(kv.mod_revision, 9);
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_watch_line_created_ack_yields_no_events() {
        let events = parse_watch_line(r#"{"result":{"created":true}}"#).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_watch_line_error_frame() {
        let err = parse_watch_line(r#"{"error":{"grpc_code":14,"message":"unavailable"}}"#);
        assert!(err.is_err());
    }

    #[test]
    fn test_range_response_decodes_string_int64s() {
        let raw: WireRangeResponse = serde_json::from_str(
            r#"{"header":{"revision":"12"},"kvs":[
                {"key":"L2s=","value":"dg==","create_revision":"3","mod_revision":"12","version":"4","lease":"0"}],
              "count":"1"}"#,
        )
        .unwrap();
        let kvs = decode_kvs(raw.kvs).unwrap();
        assert_eq!(kvs.len(), 1);
        assert_eq!(kvs[0].key, "/k");
        assert_eq!(kvs[0].value, "v");
        assert_eq!(kvs[0].create_revision, 3);
        assert_eq!(kvs[0].version, 4);
    }

    /// Integration smoke test — only runs when an etcd endpoint is provided.
    #[tokio::test]
    async fn test_connect_against_live_etcd() {
        let Ok(endpoint) = std::env::var("FORGEFLEET_ETCD_ENDPOINT") else {
            return; // no etcd in CI
        };
        let client = EtcdClient::connect(EtcdConfig {
            endpoints: vec![endpoint],
            ..EtcdConfig::default()
        })
        .await
        .expect("connect to etcd");
        client.status().await.expect("etcd status");
    }
}
