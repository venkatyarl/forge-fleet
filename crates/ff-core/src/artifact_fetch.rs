//! Artifact cache manager with LAN-first fetch.
//!
//! Manages a local artifact cache laid out as `<root>/<artifact>/<version>/<file>`
//! (the same tree [`crate::artifact_cache`] evicts from) and fills cache misses
//! cheapest-first, mirroring the model-library pattern in ff-agent
//! (`hf_download` for WAN pulls, `model_transfer` for LAN rsync):
//!
//! 1. [`ArtifactCacheManager::check_cache`] — hit if the file exists and (when an
//!    expected digest is known) its SHA256 matches; a mismatch counts as a miss.
//! 2. [`ArtifactCacheManager::sync_lan`] — pull from a fleet peer's cache via
//!    `rsync` over SSH, then verify SHA256.
//! 3. [`ArtifactCacheManager::download_wan`] — stream from an HTTP(S) URL,
//!    hashing while streaming.
//!
//! All fetch paths write to a `.part` temp file and atomically rename into
//! place only after verification, so readers never see a partial or corrupt
//! artifact. [`ArtifactCacheManager::ensure_artifact`] chains all three.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

/// Blobs strictly larger than this are routed to the MinIO backend on save
/// instead of the local cache tree (1 MiB).
pub const MINIO_ROUTE_THRESHOLD_BYTES: u64 = 1024 * 1024;

/// Object-storage backend for large artifact blobs (MinIO / any
/// S3-compatible store). Keys are content-addressed by the caller, so a
/// `put_object` for an already-present key may be a no-op.
#[async_trait]
pub trait MinioBackend: Send + Sync {
    /// Upload `bytes` under `key`.
    async fn put_object(&self, key: &str, bytes: &[u8]) -> Result<()>;
}

/// Where a saved artifact's blob ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredBlob {
    /// Small blob written into the local cache tree.
    LocalPath(PathBuf),
    /// Large blob uploaded to MinIO under this content-addressed object key.
    MinioKey(String),
}

/// Metadata record for one saved artifact blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub artifact: String,
    pub version: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub location: StoredBlob,
}

/// Where a fetched artifact ultimately came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchSource {
    /// Already present (and verified) in the local cache.
    Cache,
    /// Pulled over the LAN from the named peer host.
    Lan(String),
    /// Downloaded from the WAN URL.
    Wan,
}

/// A fleet peer whose artifact cache we can rsync from.
#[derive(Debug, Clone)]
pub struct LanPeer {
    pub ssh_user: String,
    pub host: String,
    /// Absolute artifact-cache root on the peer (same layout as ours).
    pub cache_root: String,
}

impl LanPeer {
    /// Build the `user@host:'<root>/<artifact>/<version>/<file>'` rsync source
    /// spec. The remote path is single-quoted because rsync hands it to the
    /// remote shell.
    fn rsync_source_spec(&self, artifact: &str, version: &str, file_name: &str) -> String {
        let remote_path = format!(
            "{}/{artifact}/{version}/{file_name}",
            self.cache_root.trim_end_matches('/')
        );
        format!(
            "{}@{}:{}",
            self.ssh_user,
            self.host,
            shell_quote(&remote_path)
        )
    }
}

/// Conservative single-quote shell quoting for remote paths
/// (mirrors `model_transfer::shell_quote`).
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Manages the local artifact cache: hit checks plus WAN/LAN fills.
#[derive(Debug, Clone)]
pub struct ArtifactCacheManager {
    root: PathBuf,
}

impl ArtifactCacheManager {
    /// Create a manager over the cache rooted at `root` (created lazily on
    /// first fetch).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Create a manager for the canonical `~/.forgefleet/cache/artifacts`
    /// tree, creating the cache root when it does not exist.
    pub fn from_default_root() -> Result<Self> {
        let root = default_artifact_cache_root();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("create artifact cache root {}", root.display()))?;
        Ok(Self::new(root))
    }

    /// Cache root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Final on-disk path for one cached artifact file.
    pub fn artifact_path(&self, artifact: &str, version: &str, file_name: &str) -> PathBuf {
        self.root.join(artifact).join(version).join(file_name)
    }

    /// Check the local cache. Returns the path on a verified hit.
    ///
    /// When `expected_sha256` is given the file is hashed; a mismatch is
    /// logged and treated as a miss (returns `Ok(None)`) so callers re-fetch
    /// rather than serve a corrupt artifact. A missing file is a plain miss.
    pub fn check_cache(
        &self,
        artifact: &str,
        version: &str,
        file_name: &str,
        expected_sha256: Option<&str>,
    ) -> Result<Option<PathBuf>> {
        let path = self.artifact_path(artifact, version, file_name);
        if !path.is_file() {
            return Ok(None);
        }
        if let Some(expected) = expected_sha256 {
            let actual = sha256_file(&path)
                .with_context(|| format!("hash cached artifact {}", path.display()))?;
            if !actual.eq_ignore_ascii_case(expected) {
                warn!(
                    path = %path.display(),
                    expected,
                    actual = %actual,
                    "artifact_fetch: cached artifact failed SHA256 check — treating as miss"
                );
                return Ok(None);
            }
        }
        Ok(Some(path))
    }

    /// Download an artifact from a WAN URL into the cache, hashing while
    /// streaming. Writes to a `.part` file and renames into place only after
    /// the digest (when given) verifies.
    pub async fn download_wan(
        &self,
        url: &str,
        artifact: &str,
        version: &str,
        file_name: &str,
        expected_sha256: Option<&str>,
    ) -> Result<PathBuf> {
        let dest = self.artifact_path(artifact, version, file_name);
        let tmp = part_path(&dest);
        prepare_dest_dir(&dest)?;

        let resp = reqwest::get(url)
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url} returned HTTP {}", resp.status());
        }

        let mut resp = resp;
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("create {}", tmp.display()))?;
        let mut hasher = Sha256::new();
        let mut bytes_done = 0u64;
        while let Some(chunk) = resp
            .chunk()
            .await
            .with_context(|| format!("stream body of {url}"))?
        {
            hasher.update(&chunk);
            bytes_done += chunk.len() as u64;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("write {}", tmp.display()))?;
        }
        file.flush().await?;
        drop(file);

        let actual = hex_encode(&hasher.finalize());
        if let Some(expected) = expected_sha256 {
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(&tmp);
                bail!(
                    "WAN download of {url} failed SHA256 check: expected {expected}, got {actual}"
                );
            }
        }
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
        info!(
            url,
            path = %dest.display(),
            bytes = bytes_done,
            sha256 = %actual,
            "artifact_fetch: WAN download complete"
        );
        Ok(dest)
    }

    /// Sync an artifact from a LAN peer's cache via rsync-over-SSH, then
    /// verify SHA256. Pulls into a `.part` file (`--partial` keeps resume
    /// state across retries) and renames into place after verification.
    pub async fn sync_lan(
        &self,
        peer: &LanPeer,
        artifact: &str,
        version: &str,
        file_name: &str,
        expected_sha256: Option<&str>,
    ) -> Result<PathBuf> {
        let dest = self.artifact_path(artifact, version, file_name);
        let tmp = part_path(&dest);
        prepare_dest_dir(&dest)?;

        let source = peer.rsync_source_spec(artifact, version, file_name);
        let output = Command::new("rsync")
            .arg("-az")
            .arg("--partial")
            .arg("-e")
            .arg("ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new")
            .arg(&source)
            .arg(&tmp)
            .output()
            .await
            .with_context(|| format!("spawn rsync from {source}"))?;
        if !output.status.success() {
            bail!(
                "rsync from {source} failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        verify_and_promote(&tmp, &dest, expected_sha256)?;
        info!(
            peer = %peer.host,
            path = %dest.display(),
            "artifact_fetch: LAN sync complete"
        );
        Ok(dest)
    }

    /// Ensure an artifact is present in the cache, cheapest source first:
    /// local cache hit, then each LAN peer in order, then the WAN URL.
    ///
    /// Returns the cached path and where it came from. Fails only when every
    /// source is exhausted.
    pub async fn ensure_artifact(
        &self,
        artifact: &str,
        version: &str,
        file_name: &str,
        peers: &[LanPeer],
        wan_url: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> Result<(PathBuf, FetchSource)> {
        if let Some(path) = self.check_cache(artifact, version, file_name, expected_sha256)? {
            return Ok((path, FetchSource::Cache));
        }

        for peer in peers {
            match self
                .sync_lan(peer, artifact, version, file_name, expected_sha256)
                .await
            {
                Ok(path) => return Ok((path, FetchSource::Lan(peer.host.clone()))),
                Err(e) => {
                    warn!(
                        peer = %peer.host,
                        artifact,
                        version,
                        error = %e,
                        "artifact_fetch: LAN sync failed — trying next source"
                    );
                }
            }
        }

        if let Some(url) = wan_url {
            let path = self
                .download_wan(url, artifact, version, file_name, expected_sha256)
                .await?;
            return Ok((path, FetchSource::Wan));
        }

        bail!(
            "artifact {artifact}/{version}/{file_name} unavailable: \
             not cached, {} LAN peer(s) failed, no WAN URL",
            peers.len()
        )
    }

    /// Save an artifact blob, routing by size: blobs over
    /// [`MINIO_ROUTE_THRESHOLD_BYTES`] are uploaded to `minio` under a
    /// content-addressed key and the returned metadata record carries that
    /// key instead of a local path; smaller blobs are written into the local
    /// cache tree (via a `.part` temp file and atomic rename, like the fetch
    /// paths) and the record carries the on-disk path.
    pub async fn save_artifact(
        &self,
        artifact: &str,
        version: &str,
        file_name: &str,
        bytes: &[u8],
        minio: &dyn MinioBackend,
    ) -> Result<ArtifactRecord> {
        let size_bytes = bytes.len() as u64;
        let sha256 = hex_encode(&Sha256::digest(bytes));

        let location = if size_bytes > MINIO_ROUTE_THRESHOLD_BYTES {
            let key = format!("artifacts/{sha256}/{file_name}");
            minio
                .put_object(&key, bytes)
                .await
                .with_context(|| format!("upload artifact {artifact}/{version} to MinIO"))?;
            info!(
                artifact,
                version,
                object_key = %key,
                bytes = size_bytes,
                "artifact_fetch: large artifact routed to MinIO"
            );
            StoredBlob::MinioKey(key)
        } else {
            let dest = self.artifact_path(artifact, version, file_name);
            let tmp = part_path(&dest);
            prepare_dest_dir(&dest)?;
            std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, &dest)
                .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
            StoredBlob::LocalPath(dest)
        };

        Ok(ArtifactRecord {
            artifact: artifact.to_string(),
            version: version.to_string(),
            file_name: file_name.to_string(),
            size_bytes,
            sha256,
            location,
        })
    }
}

/// Canonical local artifact-cache root.
pub fn default_artifact_cache_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".forgefleet")
        .join("cache")
        .join("artifacts")
}

/// Temp-file path used while a fetch is in flight (`<file>.part` alongside
/// the destination, so the rename is atomic on the same filesystem).
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

/// Create the destination's parent directory tree.
fn prepare_dest_dir(dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create cache dir {}", parent.display()))?;
    }
    Ok(())
}

/// Verify a fetched temp file against `expected_sha256` (when given) and
/// atomically rename it into place. On mismatch the temp file is removed and
/// an error returned, so a corrupt fetch never becomes visible in the cache.
fn verify_and_promote(tmp: &Path, dest: &Path, expected_sha256: Option<&str>) -> Result<()> {
    if let Some(expected) = expected_sha256 {
        let actual =
            sha256_file(tmp).with_context(|| format!("hash fetched file {}", tmp.display()))?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_file(tmp);
            bail!(
                "fetched artifact {} failed SHA256 check: expected {expected}, got {actual}",
                dest.display()
            );
        }
    }
    std::fs::rename(tmp, dest)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
    Ok(())
}

/// Compute SHA256 of a file via streaming 64 KiB reads (mirrors
/// `hf_download::compute_file_sha256` — artifacts can be multi-GB, so we
/// never load the whole file). Returns the lowercase hex digest.
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {} while hashing", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_bytes(data: &[u8]) -> String {
        hex_encode(&Sha256::digest(data))
    }

    fn manager() -> (tempfile::TempDir, ArtifactCacheManager) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = ArtifactCacheManager::new(dir.path().to_path_buf());
        (dir, mgr)
    }

    #[test]
    fn artifact_path_layout_matches_eviction_tree() {
        let (_dir, mgr) = manager();
        assert_eq!(
            mgr.artifact_path("ff-agent", "v1.2.3", "ff-agent.bin"),
            mgr.root()
                .join("ff-agent")
                .join("v1.2.3")
                .join("ff-agent.bin")
        );
    }

    #[test]
    fn default_cache_root_uses_forgefleet_cache_tree() {
        let root = default_artifact_cache_root();
        assert!(root.ends_with(".forgefleet/cache/artifacts"));
    }

    #[test]
    fn check_cache_miss_when_absent() {
        let (_dir, mgr) = manager();
        assert_eq!(mgr.check_cache("a", "v1", "bin", None).unwrap(), None);
    }

    #[test]
    fn check_cache_hit_without_digest() {
        let (_dir, mgr) = manager();
        let path = mgr.artifact_path("a", "v1", "bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"payload").unwrap();
        assert_eq!(mgr.check_cache("a", "v1", "bin", None).unwrap(), Some(path));
    }

    #[test]
    fn check_cache_verifies_sha256() {
        let (_dir, mgr) = manager();
        let path = mgr.artifact_path("a", "v1", "bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"payload").unwrap();

        let good = sha256_bytes(b"payload");
        assert_eq!(
            mgr.check_cache("a", "v1", "bin", Some(&good)).unwrap(),
            Some(path.clone())
        );
        // Uppercase digests match too.
        assert_eq!(
            mgr.check_cache("a", "v1", "bin", Some(&good.to_uppercase()))
                .unwrap(),
            Some(path.clone())
        );
        // Mismatch is a miss, not an error — the file stays for re-fetch overwrite.
        let bad = sha256_bytes(b"other");
        assert_eq!(mgr.check_cache("a", "v1", "bin", Some(&bad)).unwrap(), None);
        assert!(path.exists());
    }

    #[test]
    fn verify_and_promote_renames_on_match() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bin");
        let tmp = part_path(&dest);
        std::fs::write(&tmp, b"payload").unwrap();

        verify_and_promote(&tmp, &dest, Some(&sha256_bytes(b"payload"))).unwrap();
        assert!(dest.exists());
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
    }

    #[test]
    fn verify_and_promote_removes_tmp_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bin");
        let tmp = part_path(&dest);
        std::fs::write(&tmp, b"corrupt").unwrap();

        let err = verify_and_promote(&tmp, &dest, Some(&sha256_bytes(b"payload"))).unwrap_err();
        assert!(err.to_string().contains("SHA256"));
        assert!(!tmp.exists());
        assert!(!dest.exists());
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(
            part_path(Path::new("/cache/a/v1/bin.tar.gz")),
            Path::new("/cache/a/v1/bin.tar.gz.part")
        );
    }

    #[test]
    fn rsync_source_spec_quotes_remote_path() {
        let peer = LanPeer {
            ssh_user: "bob".into(),
            host: "10.0.0.2".into(),
            cache_root: "/home/bob/.forgefleet/artifacts/".into(),
        };
        assert_eq!(
            peer.rsync_source_spec("ff-agent", "v1", "ff-agent.bin"),
            "bob@10.0.0.2:'/home/bob/.forgefleet/artifacts/ff-agent/v1/ff-agent.bin'"
        );
    }

    #[test]
    fn shell_quote_handles_apostrophes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[tokio::test]
    async fn sync_lan_fails_cleanly_when_rsync_cannot_reach_peer() {
        let (_dir, mgr) = manager();
        let peer = LanPeer {
            ssh_user: "nobody".into(),
            // TEST-NET-1 (RFC 5737) — never routable; ConnectTimeout bounds the wait.
            host: "192.0.2.1".into(),
            cache_root: "/nonexistent".into(),
        };
        let err = mgr
            .sync_lan(&peer, "a", "v1", "bin", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("rsync"));
        assert!(!mgr.artifact_path("a", "v1", "bin").exists());
    }

    #[tokio::test]
    async fn ensure_artifact_prefers_cache_hit() {
        let (_dir, mgr) = manager();
        let path = mgr.artifact_path("a", "v1", "bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"payload").unwrap();

        let (got, source) = mgr
            .ensure_artifact("a", "v1", "bin", &[], None, Some(&sha256_bytes(b"payload")))
            .await
            .unwrap();
        assert_eq!(got, path);
        assert_eq!(source, FetchSource::Cache);
    }

    /// Records uploads instead of talking to a real MinIO.
    #[derive(Default)]
    struct MockMinio {
        uploads: std::sync::Mutex<Vec<(String, usize)>>,
    }

    #[async_trait]
    impl MinioBackend for MockMinio {
        async fn put_object(&self, key: &str, bytes: &[u8]) -> Result<()> {
            self.uploads
                .lock()
                .unwrap()
                .push((key.to_string(), bytes.len()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn save_artifact_small_blob_stays_local() {
        let (_dir, mgr) = manager();
        let minio = MockMinio::default();

        let record = mgr
            .save_artifact("a", "v1", "bin", b"payload", &minio)
            .await
            .unwrap();

        let path = mgr.artifact_path("a", "v1", "bin");
        assert_eq!(record.location, StoredBlob::LocalPath(path.clone()));
        assert_eq!(record.size_bytes, 7);
        assert_eq!(record.sha256, sha256_bytes(b"payload"));
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert!(minio.uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_artifact_at_threshold_stays_local() {
        let (_dir, mgr) = manager();
        let minio = MockMinio::default();
        let blob = vec![0u8; MINIO_ROUTE_THRESHOLD_BYTES as usize];

        let record = mgr
            .save_artifact("a", "v1", "bin", &blob, &minio)
            .await
            .unwrap();

        assert!(matches!(record.location, StoredBlob::LocalPath(_)));
        assert!(minio.uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_artifact_large_blob_routes_to_minio() {
        let (_dir, mgr) = manager();
        let minio = MockMinio::default();
        let blob = vec![0u8; MINIO_ROUTE_THRESHOLD_BYTES as usize + 1];

        let record = mgr
            .save_artifact("a", "v1", "bin", &blob, &minio)
            .await
            .unwrap();

        let expected_key = format!("artifacts/{}/bin", sha256_bytes(&blob));
        assert_eq!(record.location, StoredBlob::MinioKey(expected_key.clone()));
        assert_eq!(record.size_bytes, blob.len() as u64);
        // The blob went to MinIO, not the local cache tree.
        assert!(!mgr.artifact_path("a", "v1", "bin").exists());
        assert_eq!(
            *minio.uploads.lock().unwrap(),
            vec![(expected_key, blob.len())]
        );
    }

    #[tokio::test]
    async fn ensure_artifact_errors_when_all_sources_exhausted() {
        let (_dir, mgr) = manager();
        let err = mgr
            .ensure_artifact("a", "v1", "bin", &[], None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unavailable"));
    }
}
