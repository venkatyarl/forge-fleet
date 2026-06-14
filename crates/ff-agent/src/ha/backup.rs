//! Backup orchestrator — runs on the leader, snapshots Postgres +
//! Redis on a cadence, and distributes each snapshot across the
//! fleet via the deferred-task queue (rsync fan-out).
//!
//! ## Cadence
//! - Postgres: every `postgres_interval_hours` (default 4h) via
//!   `pg_basebackup -Ft -z` streamed to a local `.tar.gz`.
//! - Redis:    every `redis_interval_hours` (default 2h) via
//!   `BGSAVE` + copy of `dump.rdb` + `zstd` compression.
//!
//! Both flows share the same post-processing:
//!   1. Compute SHA256 of the final artifact.
//!   2. INSERT into the `backups` table (schema V14) with
//!      `retention_tier='recent'`.
//!   3. Enqueue one `rsync` deferred task per member computer (trigger
//!      `node_online`) so they pick it up whenever they're next awake —
//!      EXCEPT peers that still have a pending/running backup-rsync of this
//!      kind, which are skipped so a slow peer can't accumulate a herd of
//!      concurrent transfers (coalesced to ≤1 in-flight per peer per kind).
//!   4. Run retention compaction: keep at most 2 `recent`, promote
//!      oldest to `daily`, collapse `daily` → `weekly`.
//!
//! ## Leader gating
//! `run_once` checks `pg_get_current_leader()` and short-circuits with
//! `BackupOutcome::NotLeader` when we're not the current leader. That
//! lets the spawn loop run on every daemon without coordination — only
//! the leader actually shells out to `pg_basebackup`.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use ff_db::leader_state::pg_get_current_leader;
use ff_db::pg_enqueue_deferred;
use ff_db::{pg_get_secret, pg_set_secret};

/// fleet_secrets key that stores the age X25519 recipient (public key).
/// Readable by every fleet node — encryption only.
pub const BACKUP_ENC_PUBKEY: &str = "backup_encryption_pubkey";
/// fleet_secrets key that stores the age X25519 identity (private key).
/// Only operators performing a restore should fetch this.
pub const BACKUP_ENC_PRIVKEY: &str = "backup_encryption_privkey";

const DEFAULT_POSTGRES_INTERVAL_HOURS: u64 = 4;
const DEFAULT_REDIS_INTERVAL_HOURS: u64 = 2;
/// Delay before the first tick after daemon startup (avoid racing
/// Postgres/Redis containers still coming up).
const STARTUP_DELAY_SECS: u64 = 300;
/// Retention: keep this many `retention_tier='recent'` rows before
/// promoting the oldest to `daily`.
const RECENT_RETENTION: usize = 2;
/// Retention: keep this many `daily` rows before promoting to `weekly`.
const DAILY_RETENTION: usize = 7;
/// Retention: keep this many `weekly` rows before deleting.
const WEEKLY_RETENTION: usize = 4;

/// Local on-disk file retention, applied per node to its OWN
/// `~/.forgefleet/backups/<kind>/` directory — independent of the DB
/// `backups` catalog (which only tracks the leader's rows).
///
/// The rsync fan-out drops a copy of every snapshot on every peer, but
/// nothing ever pruned those copies, so a small-disk host accumulated
/// hundreds of generations until its disk filled (ace: 226 postgres
/// snapshots ≈ 40 GiB). These caps bound that growth.
///
/// The LEADER keeps at least the DB-catalog depth
/// (`RECENT + DAILY + WEEKLY` = 13) so a count-based prune never orphans
/// a `backups` row (the catalog only ever references the most-recent ≤13
/// generations). PEERS hold only a few most-recent generations as
/// disaster-recovery replicas.
const LOCAL_KEEP_POSTGRES_LEADER: usize = 14;
const LOCAL_KEEP_POSTGRES_PEER: usize = 4;
const LOCAL_KEEP_REDIS_LEADER: usize = 60;
const LOCAL_KEEP_REDIS_PEER: usize = 24;
/// How often each node prunes its own backup directory.
const PRUNE_INTERVAL_SECS: u64 = 3600;

/// Errors emitted by [`BackupOrchestrator`].
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backup cmd failed: {0}")]
    Cmd(String),
    #[error("uuid: {0}")]
    Uuid(#[from] uuid::Error),
    #[error("unknown backup kind: {0}")]
    UnknownKind(String),
}

/// Result of a single [`BackupOrchestrator::run_once`] cycle.
#[derive(Debug, Clone)]
pub struct BackupReport {
    pub kind: String,
    pub file_name: String,
    pub file_path: PathBuf,
    pub size_bytes: i64,
    pub sha256: String,
    pub backup_id: Uuid,
    /// Member computer names we enqueued an rsync task for.
    pub distributed_to: Vec<String>,
    /// True if we're the leader and actually produced a backup.
    /// False when `run_once` short-circuits because we're not leader.
    pub produced: bool,
}

impl BackupReport {
    /// Construct a "skipped because not leader" report.
    pub fn not_leader(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            file_name: String::new(),
            file_path: PathBuf::new(),
            size_bytes: 0,
            sha256: String::new(),
            backup_id: Uuid::nil(),
            distributed_to: Vec::new(),
            produced: false,
        }
    }
}

/// Periodic backup runner.
#[derive(Clone)]
pub struct BackupOrchestrator {
    pg: PgPool,
    my_computer_id: Uuid,
    my_node_name: String,
    backup_dir: PathBuf,
    postgres_interval_hours: u64,
    redis_interval_hours: u64,
}

impl BackupOrchestrator {
    /// Build a new orchestrator with default intervals (4h pg, 2h redis).
    /// `backup_dir` defaults to `~/.forgefleet/backups/` if None.
    pub fn new(
        pg: PgPool,
        my_computer_id: Uuid,
        my_node_name: String,
        backup_dir: Option<PathBuf>,
    ) -> Self {
        let default_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".forgefleet/backups");
        Self {
            pg,
            my_computer_id,
            my_node_name,
            backup_dir: backup_dir.unwrap_or(default_dir),
            postgres_interval_hours: DEFAULT_POSTGRES_INTERVAL_HOURS,
            redis_interval_hours: DEFAULT_REDIS_INTERVAL_HOURS,
        }
    }

    /// Override the postgres cadence (hours).
    pub fn with_postgres_interval(mut self, hours: u64) -> Self {
        self.postgres_interval_hours = hours.max(1);
        self
    }

    /// Override the redis cadence (hours).
    pub fn with_redis_interval(mut self, hours: u64) -> Self {
        self.redis_interval_hours = hours.max(1);
        self
    }

    /// Run a single backup cycle for the given kind (`"postgres"`,
    /// `"redis"`, or `"all"`). Silently short-circuits when we're not
    /// the current fleet leader *unless* `force` is true.
    pub async fn run_once(
        &self,
        kind: &str,
        force: bool,
    ) -> Result<Vec<BackupReport>, BackupError> {
        if !force && !self.i_am_leader().await? {
            debug!(node = %self.my_node_name, kind, "backup skipped — not leader");
            return Ok(vec![BackupReport::not_leader(kind)]);
        }

        let kinds: Vec<&str> = match kind {
            "all" => vec!["postgres", "redis"],
            "postgres" | "redis" => vec![kind],
            other => return Err(BackupError::UnknownKind(other.to_string())),
        };

        let mut reports = Vec::new();
        for k in kinds {
            let report = match k {
                "postgres" => self.run_postgres().await?,
                "redis" => self.run_redis().await?,
                _ => unreachable!(),
            };
            // Retention compaction is per-kind.
            if let Err(e) = self.run_retention(k).await {
                warn!(kind = k, error = %e, "retention compaction failed");
            }
            reports.push(report);
        }
        // Prune our own on-disk copies after producing/cataloguing. Runs
        // on the leader here; peers prune via the spawn-loop ticker (they
        // short-circuit above and never reach this point).
        self.prune_all_local().await;
        // Leader-driven backup-replica reaper: prune OVER-QUOTA peers' excess
        // backup copies via an embedded shell script. The per-node prune above
        // (and the peer-side prune ticker) only works on hosts running a binary
        // that HAS the prune code (#210); a host stuck on an older binary
        // accumulates backups forever, fills its disk, and can no longer build
        // the very upgrade that ships the prune — a deadlock (observed: ace
        // stuck on #116/May-31 with 42 GiB of un-pruned postgres replicas).
        // disk_reconcile actuates disk pressure but only evicts MODELS, so it's
        // blind to backup replicas. This reaper closes that gap: the prune
        // script is generated on the leader and SSH-executed on the peer, so it
        // works regardless of the peer's binary version.
        self.reap_over_quota_peers().await;
        Ok(reports)
    }

    /// Spawn the periodic backup loop. Runs forever until `shutdown`
    /// flips to true. Waits [`STARTUP_DELAY_SECS`] before the first
    /// tick to let the Postgres / Redis containers stabilize.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                node = %self.my_node_name,
                pg_hours = self.postgres_interval_hours,
                redis_hours = self.redis_interval_hours,
                dir = %self.backup_dir.display(),
                "backup orchestrator starting; initial delay {}s",
                STARTUP_DELAY_SECS
            );

            // Initial delay.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SECS)) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }

            let pg_period = Duration::from_secs(self.postgres_interval_hours * 3600);
            let redis_period = Duration::from_secs(self.redis_interval_hours * 3600);
            let mut pg_ticker = tokio::time::interval(pg_period);
            let mut redis_ticker = tokio::time::interval(redis_period);
            // Prune our OWN backup dir on a cadence regardless of leader
            // status — peers never produce backups but DO receive rsync'd
            // copies that would otherwise accumulate forever. First tick
            // fires immediately after the startup delay.
            let mut prune_ticker = tokio::time::interval(Duration::from_secs(PRUNE_INTERVAL_SECS));
            // Both start "due now" — fire once immediately after the
            // startup delay, then on cadence.
            pg_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            redis_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            prune_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = pg_ticker.tick() => {
                        match self.run_once("postgres", false).await {
                            Ok(reports) => {
                                for r in &reports {
                                    if r.produced {
                                        info!(
                                            kind = %r.kind,
                                            file = %r.file_name,
                                            size = r.size_bytes,
                                            targets = r.distributed_to.len(),
                                            "backup produced"
                                        );
                                    }
                                }
                            }
                            Err(e) => error!(error = %e, "postgres backup tick failed"),
                        }
                    }
                    _ = redis_ticker.tick() => {
                        match self.run_once("redis", false).await {
                            Ok(reports) => {
                                for r in &reports {
                                    if r.produced {
                                        info!(
                                            kind = %r.kind,
                                            file = %r.file_name,
                                            size = r.size_bytes,
                                            targets = r.distributed_to.len(),
                                            "backup produced"
                                        );
                                    }
                                }
                            }
                            Err(e) => error!(error = %e, "redis backup tick failed"),
                        }
                    }
                    _ = prune_ticker.tick() => {
                        self.prune_all_local().await;
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            info!("backup orchestrator shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }

    // ─── Postgres ─────────────────────────────────────────────────────

    async fn run_postgres(&self) -> Result<BackupReport, BackupError> {
        let out_dir = self.backup_dir.join("postgres");
        tokio::fs::create_dir_all(&out_dir).await?;

        // Ensure an age keypair is provisioned in fleet_secrets before we
        // try to encrypt. If the operator hasn't set one, we generate a
        // fresh X25519 keypair and store it.
        let recipient = ensure_backup_keypair(&self.pg).await?;

        // Ensure the `replicator` Postgres role exists before we shell
        // pg_basebackup -U replicator. On a fresh / post-wipe DB the role is
        // absent and every tick fails with `role "replicator" does not exist`
        // (observed spamming the leader's daemon log after the Apr-18 DB
        // wipe). Self-heal it here, mirroring ensure_backup_keypair, so the HA
        // backup never depends on an operator hand-running
        // deploy/sql/setup-replication.sql.
        ensure_replication_role(&self.pg).await?;

        let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let file_name = format!("pg-{ts}.tar.gz.age");
        let path = out_dir.join(&file_name);

        // pg_basebackup -Ft -z writes a tar+gzip stream to stdout when
        // -D is "-". `-X fetch` is required with "-D -" (streaming WAL
        // to stdout is incompatible with tar-to-stdout). We stream
        // that through the `age` CLI (-r <recipient>) for encryption,
        // and write the ciphertext to path (`pg-*.tar.gz.age`).
        //
        // Using /bin/sh -c so the pipeline is handled by the shell and
        // we don't need to wire three tokio Commands together.
        let shell_cmd = format!(
            "docker exec -e PGPASSWORD={pw} forgefleet-postgres \
                 pg_basebackup -h 127.0.0.1 -U replicator -D - -Ft -z -X fetch \
                 | age -r {recipient} > {out}",
            pw = REPLICATOR_DEFAULT_PASSWORD,
            recipient = shell_quote(&recipient),
            out = shell_quote(&path.to_string_lossy()),
        );

        info!(path = %path.display(), "running pg_basebackup | age");
        let status = run_pipeline(&shell_cmd).await?;
        if !status.success() {
            return Err(BackupError::Cmd(format!(
                "pg_basebackup|age pipeline exited with status {status}; \
                 is the `age` CLI installed on this host?"
            )));
        }

        let (size_bytes, sha256) = file_metadata(&path).await?;
        validate_backup_size("postgres", &path, size_bytes).await?;
        let backup_id = self
            .insert_backup_row("postgres", &file_name, size_bytes, &sha256)
            .await?;

        let targets = self.enqueue_distribution("postgres", &file_name).await?;

        Ok(BackupReport {
            kind: "postgres".into(),
            file_name,
            file_path: path,
            size_bytes,
            sha256,
            backup_id,
            distributed_to: targets,
            produced: true,
        })
    }

    // ─── Redis ────────────────────────────────────────────────────────

    async fn run_redis(&self) -> Result<BackupReport, BackupError> {
        let out_dir = self.backup_dir.join("redis");
        tokio::fs::create_dir_all(&out_dir).await?;

        let recipient = ensure_backup_keypair(&self.pg).await?;

        let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let file_name = format!("redis-{ts}.rdb.zst.age");
        let path = out_dir.join(&file_name);

        // 1) Ask Redis to write an RDB snapshot. BGSAVE is async but
        //    with appendonly yes the RDB may trail the AOF; that's fine
        //    for our recovery model (AOF replays on restore anyway).
        let bgsave = Command::new("docker")
            .args(["exec", "forgefleet-redis", "redis-cli", "BGSAVE"])
            .output()
            .await?;
        if !bgsave.status.success() {
            return Err(BackupError::Cmd(format!(
                "redis BGSAVE failed: {}",
                String::from_utf8_lossy(&bgsave.stderr).trim()
            )));
        }

        // 2) Wait for LASTSAVE to advance (short poll — 60s max).
        let before_ts = redis_lastsave().await.unwrap_or(0);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if redis_lastsave().await.unwrap_or(0) > before_ts {
                break;
            }
        }

        // 3) Stream the dump out of the container through zstd + age
        //    into the target file. `docker cp` would copy to a temp
        //    first; a pipe is smaller-on-disk and faster.
        let shell_cmd = format!(
            "docker exec forgefleet-redis cat /data/dump.rdb \
                | zstd -q \
                | age -r {recipient} > {out}",
            recipient = shell_quote(&recipient),
            out = shell_quote(&path.to_string_lossy()),
        );
        let status = run_pipeline(&shell_cmd).await?;
        if !status.success() {
            // Fallback: try plain gzip if zstd isn't on PATH. Still
            // encrypt-at-rest via age.
            warn!("zstd unavailable or failed; falling back to gzip");
            let file_name_gz = format!("redis-{ts}.rdb.gz.age");
            let path_gz = out_dir.join(&file_name_gz);
            let shell_cmd_gz = format!(
                "docker exec forgefleet-redis cat /data/dump.rdb \
                    | gzip \
                    | age -r {recipient} > {out}",
                recipient = shell_quote(&recipient),
                out = shell_quote(&path_gz.to_string_lossy()),
            );
            let status_gz = run_pipeline(&shell_cmd_gz).await?;
            if !status_gz.success() {
                return Err(BackupError::Cmd(format!(
                    "redis dump export failed: {status_gz}"
                )));
            }
            let (size_bytes, sha256) = file_metadata(&path_gz).await?;
            validate_backup_size("redis", &path_gz, size_bytes).await?;
            let backup_id = self
                .insert_backup_row("redis", &file_name_gz, size_bytes, &sha256)
                .await?;
            let targets = self.enqueue_distribution("redis", &file_name_gz).await?;
            return Ok(BackupReport {
                kind: "redis".into(),
                file_name: file_name_gz,
                file_path: path_gz,
                size_bytes,
                sha256,
                backup_id,
                distributed_to: targets,
                produced: true,
            });
        }

        let (size_bytes, sha256) = file_metadata(&path).await?;
        validate_backup_size("redis", &path, size_bytes).await?;
        let backup_id = self
            .insert_backup_row("redis", &file_name, size_bytes, &sha256)
            .await?;
        let targets = self.enqueue_distribution("redis", &file_name).await?;

        Ok(BackupReport {
            kind: "redis".into(),
            file_name,
            file_path: path,
            size_bytes,
            sha256,
            backup_id,
            distributed_to: targets,
            produced: true,
        })
    }

    // ─── Shared helpers ───────────────────────────────────────────────

    async fn i_am_leader(&self) -> Result<bool, BackupError> {
        let cur = pg_get_current_leader(&self.pg).await?;
        Ok(cur
            .map(|l| l.member_name == self.my_node_name)
            .unwrap_or(false))
    }

    async fn insert_backup_row(
        &self,
        kind: &str,
        file_name: &str,
        size_bytes: i64,
        sha256: &str,
    ) -> Result<Uuid, BackupError> {
        let row = sqlx::query(
            "INSERT INTO backups
                (database_kind, size_bytes, source_computer_id, checksum_sha256,
                 file_name, retention_tier)
             VALUES ($1, $2, $3, $4, $5, 'recent')
             RETURNING id",
        )
        .bind(kind)
        .bind(size_bytes)
        .bind(self.my_computer_id)
        .bind(sha256)
        .bind(file_name)
        .fetch_one(&self.pg)
        .await?;
        let id: Uuid = row.get("id");
        Ok(id)
    }

    /// Enqueue one rsync deferred task per peer computer. The source
    /// IP is our current primary IP from the `computers` table.
    async fn enqueue_distribution(
        &self,
        kind: &str,
        file_name: &str,
    ) -> Result<Vec<String>, BackupError> {
        // Look up my own IP + SSH user so peers know where + as whom
        // to rsync from. REDIS.1 part 2 (2026-05-19): the original code
        // built the source as `<ip>:/path` with no user prefix, so each
        // peer's rsync tried `ssh <peer-local-user>@<my_ip>` (e.g.
        // `adele@taylor`) and Taylor's sshd rejected with exit-255
        // because there is no `adele` Unix user on Taylor.
        // Now we always prefix the leader's `ssh_user` from
        // `computers.ssh_user`, falling back to "root" if unset.
        let row =
            sqlx::query("SELECT primary_ip, COALESCE(ssh_user, 'root') AS ssh_user FROM computers WHERE id = $1")
                .bind(self.my_computer_id)
                .fetch_optional(&self.pg)
                .await?;
        let (my_ip, my_user): (String, String) = match row {
            Some(r) => {
                use sqlx::Row;
                (r.get("primary_ip"), r.get("ssh_user"))
            }
            None => ("127.0.0.1".to_string(), "root".to_string()),
        };

        // Target every computer except me that has a live `last_seen_at`
        // within the last 24h. Fresh-on-disk peers are picked up by the
        // deferred queue when they next come online.
        let rows = sqlx::query(
            "SELECT c.name
               FROM computers c
              WHERE c.id <> $1
                AND (c.last_seen_at IS NULL OR c.last_seen_at > NOW() - INTERVAL '24 hours')",
        )
        .bind(self.my_computer_id)
        .fetch_all(&self.pg)
        .await?;

        let mut enqueued = Vec::new();
        let source_path = format!(
            "{}@{}:{}/{}/{}",
            my_user,
            my_ip,
            self.backup_dir.display(),
            kind,
            file_name
        );
        // kind is always an internal literal (redis/postgres/nats/...). Filter
        // anyway so the unquoted interpolation below stays injection-safe.
        let kind_safe: String = kind
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();

        let who = format!("backup_orchestrator@{}", self.my_node_name);
        for row in rows {
            let name: String = row.get("name");

            // Coalesce the fan-out: skip a peer that still has a non-terminal
            // (pending/dispatchable/running) backup-rsync of this kind. Without this, every
            // snapshot enqueues a fresh ~1 GiB rsync to EVERY peer regardless of
            // whether the last one finished. A slow peer (priya/sophie, observed
            // 2026-06-13 with 7-9 concurrent stuck transfers) then piles up
            // simultaneous 1 GiB pulls that mutually thrash and turn the leader
            // into a thundering-herd rsync sender, so none complete before the 2h
            // stale-task reaper kills them — they retry and the cycle repeats.
            // Capping each peer's in-flight backlog to 1/kind keeps DR current
            // (a draining peer still gets a recent snapshot next cycle) while
            // eliminating the herd. Fail-open: a count error still enqueues so a
            // transient DB hiccup never silently skips a backup.
            let inflight: i64 = match sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM deferred_tasks \
                  WHERE preferred_node = $1 \
                    AND kind = 'shell' \
                    AND status IN ('pending', 'dispatchable', 'running') \
                    AND title LIKE $2",
            )
            .bind(&name)
            .bind(backup_rsync_title_like(kind))
            .fetch_one(&self.pg)
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    warn!(target_node = %name, error = %e,
                          "backup coalesce check failed; enqueueing anyway");
                    0
                }
            };
            if inflight > 0 {
                debug!(target_node = %name, kind, inflight,
                       "skipping backup-rsync fan-out — peer still draining a prior transfer of this kind");
                continue;
            }

            let title = format!("rsync {kind} backup {file_name} → {name}");
            let script = backup_rsync_script(&kind_safe, &shell_quote(&source_path));
            let payload = serde_json::json!({
                "command": script,
                "summary": title,
            });
            let trigger_spec = serde_json::json!({ "node": name });
            let required_caps = serde_json::json!([]);
            match pg_enqueue_deferred(
                &self.pg,
                &title,
                "shell",
                &payload,
                "node_online",
                &trigger_spec,
                Some(&name),
                &required_caps,
                Some(&who),
                Some(3),
            )
            .await
            {
                Ok(_id) => enqueued.push(name),
                Err(e) => {
                    warn!(target_node = %name, error = %e, "failed to enqueue rsync task");
                }
            }
        }
        Ok(enqueued)
    }

    /// 4-tier retention: recent → daily → weekly → deleted.
    ///
    /// Rule of thumb per kind:
    /// - Keep up to [`RECENT_RETENTION`] rows marked `recent`. When
    ///   that's exceeded, promote the oldest to `daily`.
    /// - Keep up to [`DAILY_RETENTION`] rows marked `daily`. Oldest
    ///   excess becomes `weekly`.
    /// - Keep up to [`WEEKLY_RETENTION`] rows marked `weekly`. Oldest
    ///   excess is deleted.
    ///
    /// The actual on-disk file for a deleted row is NOT unlinked here
    /// — that's handled by a separate sweeper so a failed DB update
    /// can't orphan data.
    async fn run_retention(&self, kind: &str) -> Result<(), BackupError> {
        // Promote excess `recent` → `daily`.
        self.promote_excess(kind, "recent", "daily", RECENT_RETENTION)
            .await?;
        // Promote excess `daily` → `weekly`.
        self.promote_excess(kind, "daily", "weekly", DAILY_RETENTION)
            .await?;
        // Delete excess `weekly`.
        self.delete_excess(kind, "weekly", WEEKLY_RETENTION).await?;
        Ok(())
    }

    async fn promote_excess(
        &self,
        kind: &str,
        from_tier: &str,
        to_tier: &str,
        keep: usize,
    ) -> Result<(), BackupError> {
        sqlx::query(
            "UPDATE backups SET retention_tier = $1
              WHERE id IN (
                  SELECT id FROM backups
                  WHERE database_kind = $2 AND retention_tier = $3
                  ORDER BY created_at DESC
                  OFFSET $4
              )",
        )
        .bind(to_tier)
        .bind(kind)
        .bind(from_tier)
        .bind(keep as i64)
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    async fn delete_excess(&self, kind: &str, tier: &str, keep: usize) -> Result<(), BackupError> {
        sqlx::query(
            "DELETE FROM backups
              WHERE id IN (
                  SELECT id FROM backups
                  WHERE database_kind = $1 AND retention_tier = $2
                  ORDER BY created_at DESC
                  OFFSET $3
              )",
        )
        .bind(kind)
        .bind(tier)
        .bind(keep as i64)
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    /// Prune local backup files for every kind, choosing retention depth
    /// by our current leader/peer role. Never fails the caller — this is
    /// best-effort disk hygiene that must not perturb the backup cadence.
    pub async fn prune_all_local(&self) {
        // Fail-open to the LEADER (larger) depth on a lookup error so a
        // transient DB blip can never cause over-deletion of replicas.
        let leader = self.i_am_leader().await.unwrap_or(true);
        let (pg_keep, redis_keep) = if leader {
            (LOCAL_KEEP_POSTGRES_LEADER, LOCAL_KEEP_REDIS_LEADER)
        } else {
            (LOCAL_KEEP_POSTGRES_PEER, LOCAL_KEEP_REDIS_PEER)
        };
        if let Err(e) = self.prune_local_backups("postgres", pg_keep).await {
            warn!(error = %e, "postgres local backup prune failed");
        }
        if let Err(e) = self.prune_local_backups("redis", redis_keep).await {
            warn!(error = %e, "redis local backup prune failed");
        }
    }

    /// Prune this node's OWN `<backup_dir>/<kind>/` directory, keeping the
    /// `keep` most-recent files (by mtime) and unlinking the rest. Only
    /// touches files whose name carries the kind's backup prefix
    /// (`pg-` / `redis-`), so an operator file dropped in the directory is
    /// never deleted. Best-effort: logs and continues past individual
    /// unlink errors. Returns `(files_removed, bytes_freed)`.
    async fn prune_local_backups(
        &self,
        kind: &str,
        keep: usize,
    ) -> Result<(u64, u64), BackupError> {
        let keep = keep.max(1);
        let prefix = kind_prefix(kind);
        if prefix.is_empty() {
            return Ok((0, 0));
        }
        let dir = self.backup_dir.join(kind);
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            // No directory yet (node never received a backup) → nothing to do.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(e.into()),
        };
        let mut files: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(prefix) {
                continue; // never touch non-backup files
            }
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((entry.path(), mtime, meta.len()));
        }
        let targets = prune_targets(files, keep);
        let mut removed = 0u64;
        let mut freed = 0u64;
        for (path, size) in targets {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    removed += 1;
                    freed += size;
                }
                Err(e) => warn!(file = %path.display(), error = %e, "backup prune: unlink failed"),
            }
        }
        if removed > 0 {
            info!(
                kind,
                removed,
                freed_bytes = freed,
                kept = keep,
                "pruned old local backups"
            );
        }
        Ok((removed, freed))
    }

    /// Leader-driven over-quota backup-replica reaper.
    ///
    /// For every peer currently OVER its disk quota (same definition as
    /// `disk_reconcile::over_quota_nodes`), enqueue a self-contained shell task
    /// that prunes that peer's `~/.forgefleet/backups/<kind>/` directory down to
    /// the peer retention floor. The script carries no `ff`/`forgefleetd`
    /// dependency, so it heals a peer whose binary predates the in-process prune
    /// (#210) — exactly the host that would otherwise deadlock (disk full → can't
    /// build the upgrade that ships the prune).
    ///
    /// Skips the leader itself (it prunes locally via `prune_all_local`) and
    /// coalesces: a peer that already has a non-terminal reap of this kind queued
    /// is left alone, so repeated backup ticks never pile up duplicate reaps.
    /// Best-effort — never propagates an error (disk hygiene must not perturb the
    /// backup cadence).
    async fn reap_over_quota_peers(&self) {
        let over = match crate::disk_reconcile::over_quota_nodes(&self.pg).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "backup reaper: over-quota lookup failed");
                return;
            }
        };
        if over.is_empty() {
            return;
        }
        let who = format!("backup_reaper@{}", self.my_node_name);
        for name in over {
            // The leader prunes its OWN dir locally; never SSH-reap ourselves.
            if name.eq_ignore_ascii_case(&self.my_node_name) {
                continue;
            }
            for (kind, keep) in [
                ("postgres", LOCAL_KEEP_POSTGRES_PEER),
                ("redis", LOCAL_KEEP_REDIS_PEER),
            ] {
                // Coalesce: skip a peer that still has a pending/running reap of
                // this kind. Fail-open (a count error still enqueues).
                let inflight: i64 = match sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM deferred_tasks \
                      WHERE preferred_node = $1 \
                        AND kind = 'shell' \
                        AND status IN ('pending', 'dispatchable', 'running') \
                        AND title LIKE $2",
                )
                .bind(&name)
                .bind(reap_title_like(kind))
                .fetch_one(&self.pg)
                .await
                {
                    Ok(n) => n,
                    Err(e) => {
                        warn!(target_node = %name, error = %e,
                              "backup reaper coalesce check failed; enqueueing anyway");
                        0
                    }
                };
                if inflight > 0 {
                    continue;
                }

                let script = peer_backup_reap_script(kind, keep);
                if script.is_empty() {
                    continue;
                }
                let title = format!("reap {kind} backups on {name} (over-quota self-heal)");
                let payload = serde_json::json!({ "command": script, "summary": title });
                // "now" trigger + preferred_node: any worker (including the
                // leader's, which is guaranteed to run the new binary) can claim
                // it and SSH the reap to the peer — so a wedged peer worker can't
                // block its own disk rescue.
                match pg_enqueue_deferred(
                    &self.pg,
                    &title,
                    "shell",
                    &payload,
                    "now",
                    &serde_json::json!({}),
                    Some(&name),
                    &serde_json::json!([]),
                    Some(&who),
                    Some(2),
                )
                .await
                {
                    Ok(_id) => info!(
                        target_node = %name, kind, keep,
                        "backup reaper: enqueued over-quota replica prune"
                    ),
                    Err(e) => warn!(target_node = %name, kind, error = %e,
                                    "backup reaper: failed to enqueue reap task"),
                }
            }
        }
    }
}

// ─── Free helpers ─────────────────────────────────────────────────────

async fn file_metadata(path: &Path) -> Result<(i64, String), BackupError> {
    let meta = tokio::fs::metadata(path).await?;
    let size_bytes = meta.len() as i64;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok((size_bytes, format!("{:x}", digest)))
}

async fn redis_lastsave() -> Option<u64> {
    let out = Command::new("docker")
        .args(["exec", "forgefleet-redis", "redis-cli", "LASTSAVE"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Filename prefix the backup writer uses for a kind's artifacts
/// (`pg-<ts>.tar.gz.age`, `redis-<ts>.rdb.zst.age`). Empty for an
/// unknown kind so the prune becomes a no-op rather than matching
/// everything.
fn kind_prefix(kind: &str) -> &'static str {
    match kind {
        "postgres" => "pg-",
        "redis" => "redis-",
        _ => "",
    }
}

/// SQL `LIKE` pattern matching every backup-rsync task title for `kind`
/// (the titles look like `rsync postgres backup <file> → <peer>`). Used to
/// coalesce the fan-out: a peer that still has a pending/running rsync of
/// this kind is skipped, so a slow peer never accumulates a herd of
/// concurrent ~1 GiB transfers that mutually thrash and overload the
/// leader's rsync sender. Pure so it's unit-testable against the real title
/// format produced in `enqueue_distribution`.
fn backup_rsync_title_like(kind: &str) -> String {
    format!("rsync {kind} backup %")
}

/// Run a shell pipeline with `pipefail` so a failure in ANY stage propagates,
/// not just the last one.
///
/// HA.1 (2026-06-14): the backup pipelines are `pg_basebackup | age > f` and
/// `cat dump.rdb | zstd | age > f`. Under the default `sh -c "a | b > c"` the
/// exit status is `c`'s alone, so a failed `pg_basebackup`/`cat` whose (empty)
/// stdout still encrypts cleanly through `age` looks like success — and a
/// ~184-byte ciphertext-of-nothing gets recorded as a valid backup with a real
/// SHA (observed live: `pg-20260614T132656Z.tar.gz.age`, 184 bytes). `bash` is
/// used explicitly because `pipefail` is not in POSIX `sh` (dash); the leader
/// may be macOS today or, after an HA failover, a Linux peer — both ship bash.
async fn run_pipeline(cmd: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("bash")
        .arg("-c")
        .arg(format!("set -o pipefail\n{cmd}"))
        .status()
        .await
}

/// Smallest plausible size (bytes) of a real encrypted backup artifact, by
/// kind. Belt-and-suspenders alongside [`run_pipeline`]'s `pipefail`: even if a
/// source command exits 0 but writes a truncated stream, the artifact is far
/// below these floors and we refuse to record it. Real artifacts dwarf them — a
/// postgres base backup is MB–GB even for a tiny DB (system catalogs alone),
/// and a live-fleet redis snapshot is tens of KB; a failed pipeline yields the
/// ~180–250 byte age-header-plus-nothing file.
fn min_backup_bytes(kind: &str) -> i64 {
    match kind {
        "postgres" => 4096,
        // redis (and anything else): conservative — our real snapshots are
        // ~45 KiB, a failed stream is ~200 bytes.
        _ => 512,
    }
}

/// Reject + unlink an implausibly small backup artifact so it never reaches the
/// catalog or the rsync fan-out. Returns `Ok` when the artifact clears the
/// [`min_backup_bytes`] floor for its kind. The truncated file is removed on
/// the spot (best effort) so it can't masquerade as a restore candidate or be
/// replicated fleet-wide.
async fn validate_backup_size(kind: &str, path: &Path, size_bytes: i64) -> Result<(), BackupError> {
    let floor = min_backup_bytes(kind);
    if size_bytes >= floor {
        return Ok(());
    }
    let _ = tokio::fs::remove_file(path).await;
    Err(BackupError::Cmd(format!(
        "{kind} backup artifact is implausibly small ({size_bytes} bytes < {floor} \
         floor) — the source command almost certainly failed mid-pipeline; \
         refusing to record a corrupt backup and removed {}",
        path.display()
    )))
}

/// The remote shell command a peer runs to pull one backup snapshot from the
/// leader. `kind_safe` is the backup kind (`postgres`/`redis`); `source_quoted`
/// is the already-`shell_quote`d `user@ip:/path` source. Pure so the rsync flag
/// invariants below are unit-tested against regression.
///
/// `$HOME` is expanded by the remote shell. `~` is NOT used — `shell_quote`
/// wraps the source in single quotes, which kill tilde expansion (REDIS.1,
/// 2026-05-19: rsync writing to a literal `~` dir caused exit 23 across 9
/// members).
///
/// SSH keepalive: backups can be 100s of MB; the default ssh idle timeout was
/// closing connections mid-stream (exit 255, "Connection closed by ... port 22").
///
/// NO `-z` (HA.1, 2026-06-14): every backup file is already compressed and/or
/// encrypted (`.tar.gz`, `.rdb.zst`, `.age`), so rsync's zlib pass is pure
/// overhead — it cannot shrink incompressible bytes and instead becomes the
/// throughput ceiling. Measured on the LAN: a 1.4 GiB postgres transfer took
/// 16s without `-z` vs 26s with it, burning a full CPU core on zlib (24s user
/// time vs 4s). On a contended 32 GiB Linux peer that CPU bottleneck is what
/// stalled large transfers, while ~47 KiB redis snapshots sailed through —
/// exactly the observed pattern (redis replicated fine, postgres replicas went
/// >24h stale).
///
/// SHORT `--timeout` (HA.1): an I/O-*inactivity* timer, not a wall-clock cap. A
/// healthy single-file LAN transfer never has 300s of silence, so 300s fails a
/// genuinely stalled stream 12x faster than the old 3600s. That matters because
/// `enqueue_distribution` coalesces — a peer with a pending/running rsync of
/// this kind is skipped — so one wedged transfer holding `running` for an hour
/// starves that peer of every fresh snapshot until it clears. `--partial` lets
/// the next attempt resume rather than restart.
fn backup_rsync_script(kind_safe: &str, source_quoted: &str) -> String {
    format!(
        "mkdir -p \"$HOME/.forgefleet/backups/{kind_safe}/\" && \
         rsync -a \
           -e 'ssh -o ServerAliveInterval=60 -o ServerAliveCountMax=10 -o ConnectTimeout=30' \
           --timeout=300 \
           --partial \
           {source_quoted} \"$HOME/.forgefleet/backups/{kind_safe}/\""
    )
}

/// SQL `LIKE` pattern matching every over-quota backup-reap task title for
/// `kind` (titles look like `reap <kind> backups on <peer> (...)`). Used by
/// [`BackupOrchestrator::reap_over_quota_peers`] to coalesce — a peer with an
/// in-flight reap of this kind is skipped so repeated backup ticks never pile up
/// duplicate reaps. Pure so it's unit-testable against the real title format.
fn reap_title_like(kind: &str) -> String {
    format!("reap {kind} backups %")
}

/// Build the self-contained POSIX-sh script that prunes a peer's
/// `~/.forgefleet/backups/<kind>/` directory to the newest `keep` generations.
///
/// Carries NO `ff`/`forgefleetd` dependency (plain `ls`/`tail`/`rm`), so it
/// heals a peer whose binary predates the in-process prune (#210). Only touches
/// files matching the kind's backup prefix (`pg-`/`redis-`), so an operator file
/// dropped in the directory is never deleted; an unknown kind yields an empty
/// string (caller skips it) rather than a match-all. Pure — no IO — so the
/// generated command is unit-testable.
fn peer_backup_reap_script(kind: &str, keep: usize) -> String {
    let prefix = kind_prefix(kind);
    if prefix.is_empty() {
        return String::new();
    }
    let keep = keep.max(1);
    // `ls -1t` lists newest-first (full paths, because the glob is anchored to
    // "$d"); `tail -n +<keep+1>` selects everything past the kept window. Glob
    // expansion + ls errors are swallowed so an empty/absent dir is a clean
    // no-op. `rm -f "$f"` operates on the full path ls emitted.
    let start = keep + 1;
    format!(
        "d=\"$HOME/.forgefleet/backups/{kind}\"; \
         if [ -d \"$d\" ]; then \
           ls -1t \"$d\"/{prefix}* 2>/dev/null | tail -n +{start} \
             | while IFS= read -r f; do rm -f \"$f\"; done; \
         fi; \
         echo \"backup-reap {kind} keep={keep} done\""
    )
}

/// Given `(path, mtime, size)` tuples, return the `(path, size)` of files
/// to delete: everything except the `keep` most-recent by mtime. The
/// newest files (largest mtime) are retained. Pure — no IO — so the
/// retention policy is unit-testable.
fn prune_targets(
    mut files: Vec<(PathBuf, std::time::SystemTime, u64)>,
    keep: usize,
) -> Vec<(PathBuf, u64)> {
    // Most-recent first; ties broken by path so the result is deterministic.
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    files
        .into_iter()
        .skip(keep)
        .map(|(p, _, s)| (p, s))
        .collect()
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        "''".into()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// Password the HA backup uses for the `replicator` role. Must match the
/// `PGPASSWORD=replicator-default` literal in [`BackupOrchestrator::run_postgres`]
/// and `deploy/sql/setup-replication.sql` — this is a node-local replication
/// credential reachable only over the container's loopback (pg_hba scopes it),
/// not a fleet-wide secret.
const REPLICATOR_DEFAULT_PASSWORD: &str = "replicator-default";

/// Ensure the `replicator` Postgres role exists so `pg_basebackup -U replicator`
/// can run.
///
/// The HA backup connects as `replicator`; on a fresh or post-wipe primary the
/// role is absent and every postgres backup tick fails with
/// `role "replicator" does not exist`. This idempotently provisions it (the
/// daemon connects as the DB owner/superuser, so it can `CREATE ROLE`), mirroring
/// the self-provisioning of [`ensure_backup_keypair`]. No-op when the role
/// already exists, so it's safe to call on every tick. The matching pg_hba
/// `host replication` rule ships with the Postgres container image / deploy
/// compose files, so only the role itself needs healing here.
async fn ensure_replication_role(pool: &PgPool) -> Result<(), BackupError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'replicator')")
            .fetch_one(pool)
            .await?;
    if exists {
        return Ok(());
    }

    info!("provisioning Postgres `replicator` role for HA base-backup (first-run)");
    // Password is a fixed literal (not user input) so the inline interpolation
    // is injection-safe; quoted as a SQL string literal.
    sqlx::query(&format!(
        "CREATE ROLE replicator WITH REPLICATION LOGIN PASSWORD '{REPLICATOR_DEFAULT_PASSWORD}'"
    ))
    .execute(pool)
    .await?;
    // pg_basebackup itself uses the replication protocol, but match the
    // canonical setup script so a `replicator`-authenticated logical read also
    // works for any tooling that expects it.
    sqlx::query("GRANT pg_read_all_data TO replicator")
        .execute(pool)
        .await?;
    Ok(())
}

/// Ensure the fleet has an `age` keypair available for backup encryption.
///
/// On first call (fresh DB), generates a fresh X25519 identity, stores:
///   - public key  → `fleet_secrets.backup_encryption_pubkey`
///   - private key → `fleet_secrets.backup_encryption_privkey`
///
/// Returns the recipient (public key string, `age1...`) for use with the
/// `age -r <recipient>` CLI.
pub async fn ensure_backup_keypair(pool: &PgPool) -> Result<String, BackupError> {
    if let Some(pub_k) = pg_get_secret(pool, BACKUP_ENC_PUBKEY)
        .await
        .map_err(|e| BackupError::Cmd(format!("fleet_secrets lookup: {e}")))?
    {
        // Already provisioned.
        return Ok(pub_k);
    }

    // Generate a fresh X25519 identity.
    let identity = age::x25519::Identity::generate();
    let privkey = identity.to_string();
    let pubkey = identity.to_public().to_string();

    info!(
        recipient = %pubkey,
        "provisioning fleet backup age keypair (first-run)",
    );
    pg_set_secret(
        pool,
        BACKUP_ENC_PUBKEY,
        &pubkey,
        Some("age X25519 recipient for encrypted backups"),
        Some("backup_orchestrator"),
    )
    .await
    .map_err(|e| BackupError::Cmd(format!("store pubkey: {e}")))?;

    // secrecy::ExposeSecret — we want to persist the privkey string.
    use secrecy::ExposeSecret;
    pg_set_secret(
        pool,
        BACKUP_ENC_PRIVKEY,
        privkey.expose_secret(),
        Some("age X25519 private key for decrypting backups (OPERATOR ONLY)"),
        Some("backup_orchestrator"),
    )
    .await
    .map_err(|e| BackupError::Cmd(format!("store privkey: {e}")))?;

    Ok(pubkey)
}

/// Decrypt an `.age` backup file using the fleet's stored identity.
/// Writes the plaintext to `dest` and returns `()` on success.
///
/// Reads `backup_encryption_privkey` from `fleet_secrets`. Callers must
/// have operator-level access — avoid exposing this over general RPC.
pub async fn decrypt_backup_file(
    pool: &PgPool,
    encrypted: &Path,
    dest: &Path,
) -> Result<(), BackupError> {
    let privkey_str = pg_get_secret(pool, BACKUP_ENC_PRIVKEY)
        .await
        .map_err(|e| BackupError::Cmd(format!("fleet_secrets lookup: {e}")))?
        .ok_or_else(|| {
            BackupError::Cmd("fleet_secrets.backup_encryption_privkey not set".into())
        })?;

    let identity = age::x25519::Identity::from_str(privkey_str.trim())
        .map_err(|e| BackupError::Cmd(format!("parse age identity: {e}")))?;

    let ciphertext = tokio::fs::read(encrypted).await?;
    // age 0.11 Decryptor::new is sync + returns a Decryptor that adapts
    // to Recipients or Passphrase modes. Run it off the async thread.
    let plaintext = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let decryptor = age::Decryptor::new(&ciphertext[..])
            .map_err(|e| BackupError::Cmd(format!("age decryptor: {e}")))?;
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|e| BackupError::Cmd(format!("age decrypt: {e}")))?;
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .map_err(|e| BackupError::Cmd(format!("age read: {e}")))?;
        Ok::<Vec<u8>, BackupError>(out)
    })
    .await
    .map_err(|e| BackupError::Cmd(format!("decrypt task: {e}")))??;

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(dest, plaintext).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("foo"), "'foo'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn not_leader_report_marks_produced_false() {
        let r = BackupReport::not_leader("postgres");
        assert!(!r.produced);
        assert_eq!(r.kind, "postgres");
        assert!(r.file_name.is_empty());
    }

    #[test]
    fn backup_rsync_title_like_matches_real_titles() {
        // The LIKE pattern's literal prefix must align with the real title
        // format built in enqueue_distribution, or the coalesce check silently
        // matches nothing and the herd returns.
        for kind in ["postgres", "redis"] {
            let pat = backup_rsync_title_like(kind);
            assert_eq!(pat, format!("rsync {kind} backup %"));
            let prefix = pat.trim_end_matches('%');
            let real = format!("rsync {kind} backup pg-20260613T082645Z.tar.gz.age → priya");
            assert!(
                real.starts_with(prefix),
                "pattern {pat:?} must prefix real title {real:?}"
            );
        }
        // A different kind's title must NOT match (so postgres fan-out is never
        // wrongly suppressed by an in-flight redis rsync, and vice versa).
        let pg_prefix = backup_rsync_title_like("postgres");
        let pg_prefix = pg_prefix.trim_end_matches('%');
        let redis_title = "rsync redis backup redis-20260613T094023Z.rdb.zst.age → priya";
        assert!(!redis_title.starts_with(pg_prefix));
    }

    #[test]
    fn min_backup_bytes_floors_reject_failed_pipeline_artifacts() {
        // A failed `pg_basebackup`/`cat` whose empty stdout still flows through
        // `age` yields a ~184-byte file (observed live). Both floors must sit
        // above that and far below any real artifact.
        assert!(min_backup_bytes("postgres") > 184);
        assert!(min_backup_bytes("redis") > 184);
        // postgres backups are MB-GB; redis is tens of KB — neither floor can
        // false-reject a real artifact.
        assert!(min_backup_bytes("postgres") < 100_000);
        assert!(min_backup_bytes("redis") < 45_000);
        // Unknown kinds fall back to the conservative redis floor.
        assert_eq!(
            min_backup_bytes("something-else"),
            min_backup_bytes("redis")
        );
    }

    #[tokio::test]
    async fn validate_backup_size_rejects_and_unlinks_tiny_artifacts() {
        let dir = std::env::temp_dir().join(format!("ff-bk-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let tiny = dir.join("pg-tiny.tar.gz.age");
        tokio::fs::write(&tiny, vec![0u8; 184]).await.unwrap();

        // Below the postgres floor → error AND the file is removed so it can't
        // be replicated or recorded.
        let err = validate_backup_size("postgres", &tiny, 184).await;
        assert!(err.is_err(), "184-byte postgres artifact must be rejected");
        assert!(
            !tiny.exists(),
            "rejected artifact must be unlinked, not left for the rsync fan-out"
        );

        // A plausibly-sized artifact passes and is left in place.
        let ok_path = dir.join("pg-ok.tar.gz.age");
        tokio::fs::write(&ok_path, vec![0u8; 8192]).await.unwrap();
        assert!(
            validate_backup_size("postgres", &ok_path, 8192)
                .await
                .is_ok()
        );
        assert!(ok_path.exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn backup_rsync_script_omits_compression_and_uses_short_timeout() {
        // HA.1 (2026-06-14): backup files are already compressed/encrypted, so
        // `-z` is a CPU-bound throughput ceiling that stalled large transfers.
        // Lock the flag invariants so a future edit can't silently reintroduce
        // `-z` or balloon the inactivity timeout back to an hour.
        let src = "'venkat@192.168.5.100:/Users/venkat/.forgefleet/backups/postgres/pg.tar.gz.age'";
        let script = backup_rsync_script("postgres", src);

        // Must invoke rsync WITHOUT zlib compression.
        assert!(script.contains("rsync -a "), "script: {script}");
        assert!(
            !script.contains("rsync -az") && !script.contains(" -z "),
            "rsync -z must not be reintroduced — it caps throughput on already-compressed backups: {script}"
        );
        // Short I/O-inactivity timeout so a stalled stream recovers fast.
        assert!(script.contains("--timeout=300"), "script: {script}");
        assert!(!script.contains("--timeout=3600"), "script: {script}");
        // Resume-on-retry + keepalive preserved; source + dest dir wired in.
        assert!(script.contains("--partial"), "script: {script}");
        assert!(
            script.contains("ServerAliveInterval=60"),
            "script: {script}"
        );
        assert!(script.contains(src), "source must be embedded: {script}");
        assert!(
            script.contains("\"$HOME/.forgefleet/backups/postgres/\""),
            "dest dir must be the kind-scoped backups path: {script}"
        );
    }

    #[test]
    fn reap_title_like_matches_real_reap_titles() {
        // The coalesce LIKE pattern's literal prefix must align with the title
        // built in reap_over_quota_peers, or the in-flight check silently matches
        // nothing and duplicate reaps pile up.
        for kind in ["postgres", "redis"] {
            let pat = reap_title_like(kind);
            let prefix = pat.trim_end_matches('%');
            let real = format!("reap {kind} backups on ace (over-quota self-heal)");
            assert!(
                real.starts_with(prefix),
                "pattern {pat:?} must prefix real title {real:?}"
            );
        }
        // A postgres reap must NOT match a redis reap (so one kind's in-flight
        // reap never wrongly suppresses the other's).
        let pg_prefix = reap_title_like("postgres");
        let pg_prefix = pg_prefix.trim_end_matches('%');
        let redis_title = "reap redis backups on ace (over-quota self-heal)";
        assert!(!redis_title.starts_with(pg_prefix));
    }

    #[test]
    fn peer_backup_reap_script_keeps_newest_per_kind() {
        // postgres: prefix pg-, keep 4 → delete from the 5th newest onward.
        let pg = peer_backup_reap_script("postgres", 4);
        assert!(pg.contains("backups/postgres"));
        assert!(
            pg.contains("\"$d\"/pg-*"),
            "must restrict to the pg- prefix"
        );
        assert!(pg.contains("tail -n +5"), "keep=4 → delete from line 5");
        assert!(pg.contains("rm -f \"$f\""));
        // redis: prefix redis-, keep 24 → delete from the 25th onward.
        let redis = peer_backup_reap_script("redis", 24);
        assert!(redis.contains("\"$d\"/redis-*"));
        assert!(redis.contains("tail -n +25"));
        // Unknown kind → empty (caller skips), never a match-all `rm`.
        assert!(peer_backup_reap_script("nats", 4).is_empty());
        // keep is floored at 1 so a 0 can never delete every generation.
        assert!(peer_backup_reap_script("postgres", 0).contains("tail -n +2"));
    }

    #[test]
    fn kind_prefix_maps_known_kinds() {
        assert_eq!(kind_prefix("postgres"), "pg-");
        assert_eq!(kind_prefix("redis"), "redis-");
        // Unknown kind → empty so the prune is a no-op, never a match-all.
        assert_eq!(kind_prefix("nats"), "");
    }

    /// Build a `(path, mtime, size)` tuple `secs` seconds after the epoch.
    fn f(name: &str, secs: u64) -> (PathBuf, std::time::SystemTime, u64) {
        (
            PathBuf::from(name),
            std::time::UNIX_EPOCH + Duration::from_secs(secs),
            100,
        )
    }

    #[test]
    fn prune_targets_keeps_most_recent() {
        // newest → oldest: c(30), b(20), a(10)
        let files = vec![f("a", 10), f("b", 20), f("c", 30)];
        let del = prune_targets(files, 2);
        // Keep the 2 newest (c, b); delete the oldest (a).
        assert_eq!(del.len(), 1);
        assert_eq!(del[0].0, PathBuf::from("a"));
    }

    #[test]
    fn prune_targets_noop_when_under_keep() {
        let files = vec![f("a", 10), f("b", 20)];
        assert!(prune_targets(files, 5).is_empty());
        // Exactly `keep` files → nothing deleted.
        let files = vec![f("a", 10), f("b", 20)];
        assert!(prune_targets(files, 2).is_empty());
    }

    #[test]
    fn prune_targets_deletes_all_oldest_in_order() {
        let files = vec![f("a", 10), f("b", 20), f("c", 30), f("d", 40)];
        let del = prune_targets(files, 1);
        // Keep only d(40); delete c, b, a.
        let names: Vec<_> = del.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&PathBuf::from("a")));
        assert!(names.contains(&PathBuf::from("b")));
        assert!(names.contains(&PathBuf::from("c")));
        assert!(!names.contains(&PathBuf::from("d")));
    }

    #[test]
    fn leader_local_keep_covers_db_catalog_depth() {
        // The leader's count-based prune must never orphan a `backups`
        // row: the DB catalog references at most RECENT+DAILY+WEEKLY rows,
        // which are always the most-recent generations, so keeping at
        // least that many files retains every catalogued snapshot.
        assert!(
            LOCAL_KEEP_POSTGRES_LEADER >= RECENT_RETENTION + DAILY_RETENTION + WEEKLY_RETENTION
        );
        // Peers hold strictly fewer (thin DR replicas).
        assert!(LOCAL_KEEP_POSTGRES_PEER < LOCAL_KEEP_POSTGRES_LEADER);
        assert!(LOCAL_KEEP_POSTGRES_PEER >= 1);
    }
}
