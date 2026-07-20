//! Orchestrator P3 — adaptive serving-mix autoscaler.
//!
//! A leader-gated tick that makes the deployed model mix follow LIVE demand:
//! when sessions are code-heavy it spins up MORE coder endpoints; when a kind is
//! oversupplied and its endpoints sit idle it unloads one — using ALL the
//! computers without OOMing any.
//!
//! ## Inputs / outputs (all existing primitives — no new schema)
//! - DEMAND: `ff_db::pg_recent_demand_snapshots` (the P2 contract) →
//!   `code_slots_wanted` / `general_slots_wanted`, plus the recent TREND the
//!   pre-warm anticipator projects forward (see [`anticipate`]).
//! - SUPPLY: `ff_db::pg_supplied_slots_by_kind` — healthy, agent-capable
//!   (`tool_calling` + `usable_agent_ctx >= AGENT_MIN_CTX`) deployments bucketed
//!   code vs general.
//! - PLACEMENT: `ff_db::pg_placement_candidates` (RAM/GPU/reservation per host)
//!   scored by [`score_host`]; the model to load is the cheapest tool-calling
//!   library row of the wanted kind already ON DISK on the chosen host
//!   (`ff_db::pg_loadable_library_for_kind`).
//! - ACTUATE: `model_runtime::load_model`/`unload_model` locally; the
//!   `ff model autoload --node`/`ff model unload --node` cross-node paths via the
//!   defer queue / SSH for remote hosts.
//!
//! ## SAFETY — three-mode gate (`fleet_secrets.autoscaler_mode`)
//! Read EVERY tick, exactly like auto-upgrade reads `auto_upgrade_enabled`:
//! - `off`     (DEFAULT, and the value when the key is missing): the tick does
//!              NOTHING. Deploying this is harmless.
//! - `dry-run`: compute the full load/unload plan and `tracing::info!` it, but
//!              actuate NOTHING.
//! - `active`:  actuate (reserve → load/unload → unreserve).
//!
//! Layered anti-thrash: hysteresis deadband ([`SCALE_MARGIN`]), per-kind min
//! dwell ([`MIN_DWELL_SECS`]) tracked in-memory across ticks, a fleet-wide cap on
//! concurrent loads per pass, and a never-unload-the-last-in-use-endpoint
//! invariant. Conservative by construction: it acts on at most one load and one
//! unload per pass.

use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::model_runtime::{self, AGENT_MIN_CTX, FREE_FOR_BUILD_RAM_GB, LoadOptions};

/// `fleet_secrets` key holding the three-mode gate. Off / missing = no-op.
const AUTOSCALER_MODE_KEY: &str = "autoscaler_mode";
/// Reservation owner tag — distinguishes the autoscaler's reservations from
/// other actors' so the stale-reservation reaper only clears our own orphans.
const RESERVE_OWNER: &str = "autoscaler";
/// Stale-reservation TTL: a host we reserved but never unreserved (crash /
/// failover mid-pass) is auto-released after this many seconds.
const RESERVATION_TTL_SECS: i64 = 300;

/// Hysteresis deadband: only act on a kind when |wanted − supplied| ≥ this. A
/// margin of 1.0 means a fractional fair-share demand of 0.6 never triggers a
/// load, and we never unload until the surplus is a whole endpoint.
const SCALE_MARGIN: f64 = 1.0;
/// Min dwell: don't load AND unload (or repeat the same action on) a given kind
/// more than once per this many seconds. Tracked in-process across ticks. A
/// model load takes tens of seconds to minutes, so this comfortably clears.
const MIN_DWELL_SECS: u64 = 300;
/// Cap on concurrent LOAD actions emitted per pass (fleet-wide). Conservative:
/// one stale RAM read can't oversubscribe multiple hosts.
const MAX_LOADS_PER_PASS: usize = 1;
/// An endpoint counts as idle (eligible to unload) only if its last health ping
/// is at least this old OR its request_count is 0 — a weak idle proxy until the
/// metrics-history signal is wired in.
const IDLE_HEALTH_AGE_SECS: i32 = 180;
/// Conservative per-host RAM headroom kept free (OS + ff + build). A load is
/// rejected if it would push the host past `total_ram_gb − this`.
const HOST_RAM_RESERVE_GB: f64 = 4.0;
/// Hosts kept out of autoscale churn (the leader stays free for orchestration —
/// same convention as the agent router's exclude_hosts).
const EXCLUDE_HOSTS: &[&str] = &["taylor"];

/// `fleet_secrets` keys for the per-kind RELIABILITY FLOOR — the minimum number
/// of agent-capable endpoints of each kind to keep WARM regardless of measured
/// demand. The fleet's stated P0 is agent-swarm reliability: a swarm burst must
/// always find capacity, so we keep a warm floor of code endpoints rather than
/// letting scale-down trim to a single point of failure at demand=0 (the floor
/// is meaningful only because the cross-node `autoload --node` scale-up path now
/// survives SSH-session teardown — fix ff0e9ce45). Each key is overridable per
/// fleet; missing/garbage falls back to the default below.
const CODE_FLOOR_KEY: &str = "autoscaler_code_floor";
const GENERAL_FLOOR_KEY: &str = "autoscaler_general_floor";
/// Default warm floor: TWO code endpoints (no SPOF during swarm bursts). The
/// GENERAL default is 0 because the portfolio carries no general-kind agent
/// model on disk on any non-excluded host — a non-zero general floor would just
/// log `no_fit` every tick (a futile placement attempt) and could even consume
/// the pass's single load slot away from a feasible code scale-up. Operators
/// that add a general agent model raise it via `autoscaler_general_floor`.
/// Set the code key to 1 to restore scale-to-one, or 0 to disable its floor.
const DEFAULT_CODE_FLOOR: f64 = 2.0;
const DEFAULT_GENERAL_FLOOR: f64 = 0.0;
/// Clamp ceiling for an operator-supplied floor — a typo'd `autoscaler_code_floor`
/// can't make the autoscaler try to flood the fleet with endpoints.
const MAX_FLOOR: f64 = 8.0;

// ─── Pre-warm anticipation (NEXT#0c — the swarm-burst latency lever) ──────────
// Loading an agent endpoint takes tens of seconds to minutes. Scaling on the
// single LATEST demand snapshot is therefore always one load-latency behind a
// rising burst: the endpoint finishes warming only AFTER the peak. Anticipation
// fits a slope over the recent demand snapshots and, when demand is RISING,
// projects it one horizon ahead so the load STARTS a tick early and the endpoint
// is ready WHEN the burst arrives. It is strictly one-directional: it can only
// RAISE the scale-up target, never lower it, and is bounded so a noisy spike
// can't request a flood (and MAX_LOADS_PER_PASS=1 caps actuation regardless).
/// Recency guard for the trend window: snapshots older than this are ignored, so
/// a leader-outage gap yields fewer samples (→ fall back to current) rather than
/// a stale slope. ~30 min comfortably covers several demand-tick intervals.
const TREND_LOOKBACK_SECS: i64 = 1800;
/// Max snapshots to fit the slope over — enough to smooth single-tick noise,
/// few enough to stay responsive to a genuine ramp.
const TREND_SAMPLES: i64 = 6;
/// How many snapshot-intervals ahead to project a rising trend. ~1 covers the
/// typical load latency so the endpoint is warm by the next tick.
const ANTICIPATION_HORIZON: f64 = 1.0;
/// Cap on the extra slots the projection may add over current demand. A noisy
/// spike can't ask for a flood; combined with MAX_LOADS_PER_PASS=1 the worst
/// case is one extra warm endpoint that scale-down later trims when idle.
const MAX_ANTICIPATION_SLOTS: f64 = 2.0;

// Placement scoring weights (see [`score_host`]). Named consts so they're tunable.
const W_FIT: f64 = 3.0;
const W_PERF: f64 = 4.0;
const W_IDLE: f64 = 3.0;

/// The gate's three modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoscalerMode {
    Off,
    DryRun,
    Active,
}

impl AutoscalerMode {
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => AutoscalerMode::Active,
            Some("dry-run") | Some("dry_run") | Some("dryrun") => AutoscalerMode::DryRun,
            // Off, missing, empty, or any unrecognised value → safe default.
            _ => AutoscalerMode::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AutoscalerMode::Off => "off",
            AutoscalerMode::DryRun => "dry-run",
            AutoscalerMode::Active => "active",
        }
    }
}

/// Read the gate from `fleet_secrets`. DEFAULTS TO OFF when the key is missing or
/// unparseable — so shipping this subsystem is harmless until an operator opts in.
async fn read_mode(pg: &PgPool) -> AutoscalerMode {
    match ff_db::pg_read_gate_value(pg, AUTOSCALER_MODE_KEY, "off", "off").await {
        Ok(v) => AutoscalerMode::parse(Some(v.as_str())),
        Err(e) => {
            warn!(error = %e, "autoscaler: failed to read mode secret; treating as off");
            AutoscalerMode::Off
        }
    }
}

/// Read a per-kind reliability floor from `fleet_secrets`, clamped to a sane
/// range. Missing key or unparseable value → the compiled default (so the warm
/// floor is the product's out-of-the-box behavior); a value is clamped to
/// `[0, MAX_FLOOR]` so an operator typo can't ask for an absurd endpoint count.
async fn read_floor(pg: &PgPool, key: &str, default: f64) -> f64 {
    match ff_db::pg_get_secret(pg, key).await {
        Ok(raw) => parse_floor(raw.as_deref(), default),
        Err(e) => {
            warn!(error = %e, key, "autoscaler: failed to read floor secret; using default");
            default
        }
    }
}

/// Pure parse+clamp of a reliability-floor secret value. Missing/empty/unparseable
/// → `default`; a valid number is clamped to `[0, MAX_FLOOR]`.
fn parse_floor(raw: Option<&str>, default: f64) -> f64 {
    match raw.map(str::trim) {
        Some(s) if !s.is_empty() => s
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| f.clamp(0.0, MAX_FLOOR))
            .unwrap_or(default),
        _ => default,
    }
}

/// Rank scale-up candidates most-starved-first. Pure (no dwell/feasibility
/// gating — those stay in `plan_pass`) so the deficit-ordering is unit-testable.
/// Only kinds whose deficit clears the deadband are returned, sorted by deficit
/// descending; ties keep input order (stable sort).
fn rank_deficits(items: &[(Kind, f64, i64)]) -> Vec<(Kind, f64, i64, f64)> {
    let mut out: Vec<(Kind, f64, i64, f64)> = items
        .iter()
        .map(|(k, wanted, supplied)| (*k, *wanted, *supplied, wanted - (*supplied as f64)))
        .filter(|(_, _, _, deficit)| *deficit >= SCALE_MARGIN)
        .collect();
    out.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Least-squares slope of `ys` over evenly-spaced x = 0,1,…,n−1 (one unit per
/// snapshot interval). Pure. Returns 0.0 for fewer than two points or a
/// degenerate (zero-variance) x — i.e. "no detectable trend".
fn least_squares_slope(ys: &[f64]) -> f64 {
    let n = ys.len();
    if n < 2 {
        return 0.0;
    }
    let nf = n as f64;
    let x_mean = (nf - 1.0) / 2.0;
    let y_mean = ys.iter().sum::<f64>() / nf;
    let mut num = 0.0;
    let mut den = 0.0;
    for (i, &y) in ys.iter().enumerate() {
        let dx = i as f64 - x_mean;
        num += dx * (y - y_mean);
        den += dx * dx;
    }
    if den == 0.0 { 0.0 } else { num / den }
}

/// Project demand one [`ANTICIPATION_HORIZON`] ahead from a RISING trend. Pure +
/// unit-tested. `samples` are recent demand values OLDEST→NEWEST (the last is
/// the current demand). The result is NEVER below the current value and NEVER
/// more than [`MAX_ANTICIPATION_SLOTS`] above it; a flat or FALLING trend (slope
/// ≤ 0) returns the current value unchanged — we never pre-warm on a downward
/// trend, leaving surplus to the scale-down path. Empty input → 0.0.
fn anticipate(samples: &[f64]) -> f64 {
    let Some(&current) = samples.last() else {
        return 0.0;
    };
    let slope = least_squares_slope(samples);
    if slope <= 0.0 {
        return current; // flat/falling: no pre-warm.
    }
    let projected = current + slope * ANTICIPATION_HORIZON;
    projected.clamp(current, current + MAX_ANTICIPATION_SLOTS)
}

/// Drop endpoints served by the LEADER from the supply used for floor + scale
/// decisions, returning the filtered supply and how many endpoints were dropped.
///
/// The agent-swarm router soft-excludes the leader (PR #179): the leader should
/// stay free for orchestration, so the swarm runs on non-leader workers. A
/// leader-hosted agent endpoint therefore must NOT count toward the per-kind
/// reliability FLOOR — otherwise the floor is silently satisfied by capacity the
/// swarm won't actually use, collapsing an intended floor of 2 to one usable
/// worker endpoint. Filtering here makes the floor warm non-leader capacity and
/// keeps the leader out of scale-down (we never autoscaler-unload the leader's
/// own serving). Fail-open: `leader == None` → no-op (counts everything, the
/// pre-leader-aware behavior).
fn exclude_leader_supply(
    supply: ff_db::ServingSupply,
    leader: Option<&str>,
) -> (ff_db::ServingSupply, i64) {
    let Some(leader) = leader else {
        return (supply, 0);
    };
    let before = supply.code_count + supply.general_count;
    let code_endpoints: Vec<_> = supply
        .code_endpoints
        .into_iter()
        .filter(|e| e.worker_name != leader)
        .collect();
    let general_endpoints: Vec<_> = supply
        .general_endpoints
        .into_iter()
        .filter(|e| e.worker_name != leader)
        .collect();
    let filtered = ff_db::ServingSupply {
        code_count: code_endpoints.len() as i64,
        general_count: general_endpoints.len() as i64,
        code_endpoints,
        general_endpoints,
    };
    let excluded = before - (filtered.code_count + filtered.general_count);
    (filtered, excluded)
}

/// A kind we might scale, and which workload tag it maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Code,
    General,
}

impl Kind {
    fn want_code(&self) -> bool {
        matches!(self, Kind::Code)
    }
    fn label(&self) -> &'static str {
        match self {
            Kind::Code => "code",
            Kind::General => "general",
        }
    }
}

/// One planned action — the unit of work emitted per pass.
#[derive(Debug, Clone)]
enum Action {
    Load {
        kind: Kind,
        host: String,
        library_id: String,
        catalog_id: String,
        runtime: String,
        size_gb: f64,
        score: f64,
        wanted: f64,
        supplied: i64,
    },
    Unload {
        kind: Kind,
        host: String,
        deployment_id: String,
        wanted: f64,
        supplied: i64,
    },
    /// Relaunch an already-resident, too-small, tool-capable deployment into the
    /// agent serving profile (`ff model reprofile`). The scale-up FALLBACK when no
    /// fresh load fits: it converts stuck warm capacity into an agent endpoint
    /// without needing free RAM for a new model.
    Reprofile {
        kind: Kind,
        host: String,
        deployment_id: String,
        catalog_id: String,
        wanted: f64,
        supplied: i64,
    },
}

/// Per-pass summary, for the info! log line.
#[derive(Debug, Default, Clone)]
pub struct AutoscaleSummary {
    pub mode: &'static str,
    pub code_wanted: f64,
    pub general_wanted: f64,
    /// Per-kind demand AFTER pre-warm anticipation (`max(latest, trend
    /// projection)`) but BEFORE the floor — surfaces how much the rising-trend
    /// projection lifted the raw demand. Equals `*_wanted` on a flat/falling
    /// trend.
    pub code_anticipated: f64,
    pub general_anticipated: f64,
    /// Effective per-kind target the planner scales toward:
    /// `max(anticipated, floor)`.
    pub code_target: f64,
    pub general_target: f64,
    /// Non-leader agent endpoints per kind (what the floor + scale logic uses).
    pub code_supplied: i64,
    pub general_supplied: i64,
    /// How many leader-hosted agent endpoints were dropped from the supply count
    /// before the floor comparison (they stay free for swarm orchestration).
    pub leader_excluded: i64,
    pub planned_loads: usize,
    pub planned_unloads: usize,
    /// Scale-up FALLBACK actions: reprofile an existing too-small tool-capable
    /// endpoint into the agent profile when no fresh load fit.
    pub planned_reprofiles: usize,
    pub loaded: usize,
    pub unloaded: usize,
    pub reprofiled: usize,
    pub skipped_dwell: usize,
    pub no_fit: usize,
}

/// In-process per-kind dwell tracker. The last time we ACTED on a kind; a kind
/// can't be acted on again until `MIN_DWELL_SECS` later. Process-local (the
/// leader owns the tick) — on failover the new leader starts fresh, which is
/// safe: it just gives the kind one extra dwell window.
fn dwell_state() -> &'static Mutex<HashMap<&'static str, Instant>> {
    static STATE: std::sync::OnceLock<Mutex<HashMap<&'static str, Instant>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dwell_ok(kind: Kind) -> bool {
    let map = dwell_state().lock().unwrap();
    match map.get(kind.label()) {
        Some(last) => last.elapsed() >= Duration::from_secs(MIN_DWELL_SECS),
        None => true,
    }
}

fn dwell_mark(kind: Kind) {
    dwell_state()
        .lock()
        .unwrap()
        .insert(kind.label(), Instant::now());
}

/// Whether a host's runtime can serve a model launched with `runtime`. Mirrors
/// the runtime-choice policy: mlx ⇒ macos only; vllm ⇒ CUDA/GB10 only;
/// llama.cpp ⇒ anything (CUDA/ROCm/Metal/CPU).
fn runtime_compatible(host: &ff_db::PlacementCandidate, runtime: &str) -> bool {
    let gpu = host.gpu_kind.as_deref().unwrap_or("none");
    match runtime {
        "mlx" => host.os_family == "macos",
        "vllm" => matches!(gpu, "nvidia_cuda" | "gb10"),
        // llama.cpp / ollama / anything else: assume CPU-runnable everywhere.
        _ => true,
    }
}

/// Usable inference RAM pool for a host. Unified-memory (apple_silicon / gb10)
/// AND AMD ROCm with a tiny discrete VRAM carve-out (GTT-unified, e.g. the
/// EVO-X2 boxes reporting ~2GB VRAM but 123GB RAM) use the full RAM as the pool.
/// CPU-only hosts keep more headroom (slower, and other work runs there).
fn usable_pool_gb(host: &ff_db::PlacementCandidate) -> f64 {
    let total = host.total_ram_gb.unwrap_or(0) as f64;
    let gpu = host.gpu_kind.as_deref().unwrap_or("none");
    let gtt_unified = gpu == "amd_rocm" && host.gpu_total_vram_gb.unwrap_or(0.0) < 8.0;
    let is_unified = matches!(gpu, "apple_silicon" | "gb10") || gtt_unified;
    if is_unified {
        0.75 * total
    } else if host.has_gpu && matches!(gpu, "nvidia_cuda") {
        // Discrete dGPU (none in fleet today): bounded by VRAM.
        host.gpu_total_vram_gb.unwrap_or(total)
    } else {
        0.60 * total
    }
}

/// Perf tier for the soft score: GPU compute strongly preferred for agent work
/// (a 30B CPU agent is unusably slow).
fn perf_tier(host: &ff_db::PlacementCandidate) -> f64 {
    let gpu = host.gpu_kind.as_deref().unwrap_or("none");
    match gpu {
        "gb10" | "nvidia_cuda" => 1.0,
        "apple_silicon" => 0.9,
        "amd_rocm" => 0.7,
        _ => 0.2,
    }
}

/// Pure placement scorer: `None` = host ineligible (a HARD GATE failed), else a
/// soft score where higher is better. The autoscaler picks the max-scoring host.
///
/// Hard gates: online; not excluded; reservation_state == 'available'; runtime
/// compatible; the working set FITS in free RAM (the OOM guard); and a
/// memory-tight host (≤ FREE_FOR_BUILD_RAM_GB) is never used (kept free for
/// builds). The soft score prefers healthy remaining headroom (anti-fragmentation),
/// GPU/unified compute, and least-loaded hosts (multi-session fairness).
pub fn score_host(
    host: &ff_db::PlacementCandidate,
    runtime: &str,
    working_set_gb: f64,
) -> Option<f64> {
    // ---- HARD GATES ----
    if host.status != "online" {
        return None;
    }
    if EXCLUDE_HOSTS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&host.worker_name))
    {
        return None;
    }
    if host.reservation_state != "available" {
        return None; // reserved/drained — someone else owns this host this pass.
    }
    if !runtime_compatible(host, runtime) {
        return None;
    }
    let total = host.total_ram_gb.unwrap_or(0) as f64;
    // Keep memory-tight hosts free for self-built release builds.
    if total <= FREE_FOR_BUILD_RAM_GB {
        return None;
    }
    let pool = usable_pool_gb(host);
    // free_after = usable pool minus already-resident models minus the new load.
    let free_after = pool - host.resident_model_gb - working_set_gb;
    if free_after < HOST_RAM_RESERVE_GB {
        return None; // THE OOM GUARD: would not fit with headroom.
    }

    // ---- SOFT SCORE ----
    let fit_quality = if pool > 0.0 {
        (free_after / pool).clamp(0.0, 1.0)
    } else {
        0.0
    };
    // Least-loaded: normalize active deployment count (cap at 6 for the curve).
    let load_norm = (host.active_deployments as f64 / 6.0).clamp(0.0, 1.0);
    let score = W_FIT * fit_quality + W_PERF * perf_tier(host) + W_IDLE * (1.0 - load_norm);
    Some(score)
}

/// Estimate the working-set RAM for an agent-profile load: weights × 1.15 (loader
/// overhead) + a conservative KV-cache term for the single 32K agent slot.
/// Over-estimation is safe; under-estimation re-introduces the OOM this guards.
fn agent_working_set_gb(size_gb: f64) -> f64 {
    // KV ≈ 0.5GB per 8K of single-slot ctx → ~2GB for the 32K agent slot.
    let kv = (AGENT_MIN_CTX as f64 / 8192.0) * 0.5;
    size_gb * 1.15 + kv
}

/// Compute the plan for one pass: at most one LOAD and one UNLOAD action.
/// Pure decision logic given the demand + supply + candidate snapshot — does NOT
/// actuate. Returns the actions plus the summary skeleton (numbers filled in).
async fn plan_pass(pg: &PgPool) -> Result<(Vec<Action>, AutoscaleSummary), String> {
    // Read the recent demand TREND (oldest→newest) once. The last sample is the
    // current demand; the series feeds the pre-warm anticipator. Empty (P2 not
    // producing, or a leader-outage gap) → no demand signal; the floor below
    // still applies so the fleet keeps warm agent capacity even when the sensor
    // is silent (the worst case for swarm reliability).
    let trend = ff_db::pg_recent_demand_snapshots(pg, TREND_LOOKBACK_SECS, TREND_SAMPLES)
        .await
        .map_err(|e| format!("pg_recent_demand_snapshots: {e}"))?;
    let code_series: Vec<f64> = trend.iter().map(|d| d.code_slots_wanted).collect();
    let general_series: Vec<f64> = trend.iter().map(|d| d.general_slots_wanted).collect();
    let code_wanted = code_series.last().copied().unwrap_or(0.0);
    let general_wanted = general_series.last().copied().unwrap_or(0.0);

    // PRE-WARM ANTICIPATION: when demand is rising, project it one load-latency
    // ahead so the endpoint is warm BEFORE the burst peaks. One-directional —
    // only raises the scale-up target, never lowers it (flat/falling → unchanged).
    let code_anticipated = anticipate(&code_series);
    let general_anticipated = anticipate(&general_series);

    // Apply the per-kind RELIABILITY FLOOR: the planner scales toward
    // `max(anticipated, floor)`, never below the floor. This is what keeps a warm
    // pool of agent endpoints alive at demand=0 instead of collapsing to a SPOF.
    let code_floor = read_floor(pg, CODE_FLOOR_KEY, DEFAULT_CODE_FLOOR).await;
    let general_floor = read_floor(pg, GENERAL_FLOOR_KEY, DEFAULT_GENERAL_FLOOR).await;
    let code_target = code_anticipated.max(code_floor);
    let general_target = general_anticipated.max(general_floor);

    let supply = ff_db::pg_supplied_slots_by_kind(pg, AGENT_MIN_CTX as i32)
        .await
        .map_err(|e| format!("pg_supplied_slots_by_kind: {e}"))?;

    // The reliability floor exists for SWARM redundancy, and the swarm router
    // soft-excludes the leader. So drop leader-hosted endpoints before measuring
    // supply against the floor — a leader endpoint must not let the floor read as
    // satisfied while the swarm has only one usable worker endpoint. Resolve the
    // leader from DB election state (never hardcoded); on a read failure we
    // fail-open and count everything (the pre-leader-aware behavior).
    let leader = ff_db::pg_get_current_leader(pg)
        .await
        .ok()
        .flatten()
        .map(|l| l.member_name);
    let (supply, leader_excluded) = exclude_leader_supply(supply, leader.as_deref());

    let mut summary = AutoscaleSummary {
        code_wanted,
        general_wanted,
        code_anticipated,
        general_anticipated,
        code_target,
        general_target,
        code_supplied: supply.code_count,
        general_supplied: supply.general_count,
        leader_excluded,
        ..Default::default()
    };

    let mut actions: Vec<Action> = Vec::new();

    // Evaluate both kinds against their floor-adjusted TARGET; collect deficits
    // (scale-up) and surpluses (scale-down). At most one load (the most-starved
    // kind) and one unload per pass.
    let kinds = [
        (
            Kind::Code,
            code_target,
            supply.code_count,
            &supply.code_endpoints,
        ),
        (
            Kind::General,
            general_target,
            supply.general_count,
            &supply.general_endpoints,
        ),
    ];

    // ---- SCALE-UP: most-starved kind first, falling through to the next if a
    // kind can't be placed (no loadable on-disk library for it). A perpetually
    // infeasible kind must not consume the pass's single load slot and starve a
    // kind that genuinely can scale up. ----
    let ranked = rank_deficits(
        &kinds
            .iter()
            .map(|(k, wanted, supplied, _eps)| (*k, *wanted, *supplied))
            .collect::<Vec<_>>(),
    );
    let mut tried_up = false;
    let mut placed_up = false;
    for (kind, wanted, supplied, _deficit) in ranked {
        if !dwell_ok(kind) {
            summary.skipped_dwell += 1;
            continue;
        }
        if scaleup_action_count(&actions) >= MAX_LOADS_PER_PASS {
            break;
        }
        tried_up = true;
        if let Some(act) = plan_load(pg, kind, wanted, supplied).await? {
            actions.push(act);
            placed_up = true;
            break;
        }
        // No fresh load fit for this kind. FALLBACK: convert an already-resident,
        // idle, too-small tool-capable endpoint of this kind into the agent
        // profile (reuses warm RAM — the realistic scale-up path when no host has
        // room for a new model). Purely additive: it only acts in the case the
        // autoscaler used to give up on (no_fit), so it never competes with or
        // thrashes a feasible fresh load.
        if let Some(act) = plan_reprofile(pg, kind, wanted, supplied, leader.as_deref()).await? {
            actions.push(act);
            placed_up = true;
            break;
        }
        // Neither a fresh load nor a reprofile fit; try the next-most-starved kind.
    }
    if tried_up && !placed_up {
        summary.no_fit += 1;
    }

    // ---- SCALE-DOWN: oversupplied kind whose surplus endpoints are idle ----
    // Never unload below max(1, wanted): we keep the last in-use endpoint of any
    // kind that has demand. Evaluate against the live supply (one unload/pass so
    // there's no in-pass double-decrement to track here).
    for (kind, wanted, supplied, eps) in kinds.iter() {
        let surplus = (*supplied as f64) - wanted;
        if surplus < SCALE_MARGIN {
            continue;
        }
        // Floor on how many endpoints of this kind must remain.
        let floor = (wanted.ceil() as i64).max(1);
        if *supplied <= floor {
            continue; // never drop the last in-use / wanted endpoint.
        }
        if !dwell_ok(*kind) {
            summary.skipped_dwell += 1;
            continue;
        }
        // Pick an IDLE endpoint to retire (oldest health ping / zero requests),
        // and never one on an excluded host.
        let victim = eps
            .iter()
            .filter(|e| {
                !EXCLUDE_HOSTS
                    .iter()
                    .any(|h| h.eq_ignore_ascii_case(&e.worker_name))
            })
            .filter(|e| {
                e.request_count == 0 || e.health_age_sec.unwrap_or(i32::MAX) >= IDLE_HEALTH_AGE_SECS
            })
            .max_by_key(|e| e.health_age_sec.unwrap_or(i32::MAX));
        if let Some(v) = victim {
            actions.push(Action::Unload {
                kind: *kind,
                host: v.worker_name.clone(),
                deployment_id: v.deployment_id.clone(),
                wanted: *wanted,
                supplied: *supplied,
            });
            break; // one unload per pass.
        }
    }

    summary.planned_loads = actions
        .iter()
        .filter(|a| matches!(a, Action::Load { .. }))
        .count();
    summary.planned_unloads = actions
        .iter()
        .filter(|a| matches!(a, Action::Unload { .. }))
        .count();
    summary.planned_reprofiles = actions
        .iter()
        .filter(|a| matches!(a, Action::Reprofile { .. }))
        .count();

    Ok((actions, summary))
}

/// Count the scale-UP actions in a plan (a fresh load OR a reprofile fallback).
/// Both consume the single per-pass scale-up slot, so the cap counts them
/// together — we never do more than one scale-up action per pass.
fn scaleup_action_count(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::Load { .. } | Action::Reprofile { .. }))
        .count()
}

/// Pure selection of the best reprofile candidate for `want_code`: among
/// tool-capable too-small endpoints (already filtered by the SQL), keep those NOT
/// on the leader or an excluded host and that are IDLE (no in-flight requests or a
/// stale health ping — the same idleness test scale-down uses to pick a victim, so
/// we never disrupt an endpoint that's actively serving), then take the MOST idle
/// (oldest health ping). Returns `None` when nothing qualifies.
fn pick_reprofile_candidate<'a>(
    cands: &'a [ff_db::ReprofileCandidate],
    want_code: bool,
    leader: Option<&str>,
) -> Option<&'a ff_db::ReprofileCandidate> {
    cands
        .iter()
        .filter(|c| c.is_code == want_code)
        .filter(|c| {
            !EXCLUDE_HOSTS
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&c.worker_name))
        })
        .filter(|c| leader.is_none_or(|l| !c.worker_name.eq_ignore_ascii_case(l)))
        .filter(|c| {
            c.request_count == 0 || c.health_age_sec.unwrap_or(i32::MAX) >= IDLE_HEALTH_AGE_SECS
        })
        .max_by_key(|c| c.health_age_sec.unwrap_or(i32::MAX))
}

/// Build a single REPROFILE action for `kind`: find an idle, too-small,
/// tool-capable endpoint of this kind (off the leader / excluded hosts) and
/// relaunch it in the agent profile. The scale-up FALLBACK when [`plan_load`]
/// returned `None`. Returns `None` when no such endpoint exists.
async fn plan_reprofile(
    pg: &PgPool,
    kind: Kind,
    wanted: f64,
    supplied: i64,
    leader: Option<&str>,
) -> Result<Option<Action>, String> {
    let cands = ff_db::pg_reprofile_candidates(pg, AGENT_MIN_CTX as i32)
        .await
        .map_err(|e| format!("pg_reprofile_candidates: {e}"))?;
    Ok(
        pick_reprofile_candidate(&cands, kind.want_code(), leader).map(|c| Action::Reprofile {
            kind,
            host: c.worker_name.clone(),
            deployment_id: c.deployment_id.clone(),
            catalog_id: c.catalog_id.clone().unwrap_or_default(),
            wanted,
            supplied,
        }),
    )
}

/// Build a single LOAD action for `kind`: score every candidate host, pick the
/// best one that ALSO has a loadable on-disk library row for the kind. Returns
/// `None` (no-fit) when no host both scores and has the model on disk.
async fn plan_load(
    pg: &PgPool,
    kind: Kind,
    wanted: f64,
    supplied: i64,
) -> Result<Option<Action>, String> {
    let candidates = ff_db::pg_placement_candidates(pg)
        .await
        .map_err(|e| format!("pg_placement_candidates: {e}"))?;

    // Best (score, host, library) over all candidates. We need the model on disk,
    // so we resolve the library per host and score with its real size.
    let mut best: Option<Action> = None;
    let mut best_score = f64::NEG_INFINITY;

    for host in &candidates {
        // Resolve the cheapest loadable on-disk library row of this kind here.
        let lib = ff_db::pg_loadable_library_for_kind(pg, &host.worker_name, kind.want_code())
            .await
            .map_err(|e| format!("pg_loadable_library_for_kind({}): {e}", host.worker_name))?;
        let Some((library_id, catalog_id, runtime, size_gb)) = lib else {
            continue; // model for this kind not on disk here.
        };
        let working_set = agent_working_set_gb(size_gb);
        let Some(score) = score_host(host, &runtime, working_set) else {
            continue; // a hard gate failed (OOM, runtime, reserved, …).
        };
        if score > best_score {
            best_score = score;
            best = Some(Action::Load {
                kind,
                host: host.worker_name.clone(),
                library_id,
                catalog_id,
                runtime,
                size_gb,
                score,
                wanted,
                supplied,
            });
        }
    }

    Ok(best)
}

/// Actuate one action in `active` mode: reserve → act → unreserve. The unreserve
/// always runs (even on failure) so a host can't be left stuck reserved.
async fn actuate(pg: &PgPool, action: &Action) -> Result<bool, String> {
    match action {
        Action::Load {
            host,
            library_id,
            catalog_id,
            runtime,
            ..
        } => {
            if !ff_db::pg_reserve_host(pg, host, RESERVE_OWNER)
                .await
                .map_err(|e| format!("pg_reserve_host({host}): {e}"))?
            {
                debug!(host = %host, "autoscaler: host not available to reserve; skipping load");
                return Ok(false);
            }
            let result = do_load(pg, host, runtime, library_id, catalog_id).await;
            // Always unreserve.
            if let Err(e) = ff_db::pg_unreserve_host(pg, host).await {
                warn!(host = %host, error = %e, "autoscaler: failed to unreserve host after load");
            }
            result
        }
        Action::Unload {
            host,
            deployment_id,
            ..
        } => {
            if !ff_db::pg_reserve_host(pg, host, RESERVE_OWNER)
                .await
                .map_err(|e| format!("pg_reserve_host({host}): {e}"))?
            {
                debug!(host = %host, "autoscaler: host not available to reserve; skipping unload");
                return Ok(false);
            }
            let result = do_unload(pg, host, deployment_id).await;
            if let Err(e) = ff_db::pg_unreserve_host(pg, host).await {
                warn!(host = %host, error = %e, "autoscaler: failed to unreserve host after unload");
            }
            result
        }
        Action::Reprofile {
            host,
            deployment_id,
            ..
        } => {
            if !ff_db::pg_reserve_host(pg, host, RESERVE_OWNER)
                .await
                .map_err(|e| format!("pg_reserve_host({host}): {e}"))?
            {
                debug!(host = %host, "autoscaler: host not available to reserve; skipping reprofile");
                return Ok(false);
            }
            let result = do_reprofile(pg, host, deployment_id).await;
            if let Err(e) = ff_db::pg_unreserve_host(pg, host).await {
                warn!(host = %host, error = %e, "autoscaler: failed to unreserve host after reprofile");
            }
            result
        }
    }
}

/// Perform a LOAD: locally via `model_runtime::load_model` when the host is this
/// (leader) node, else dispatch `ff model autoload <catalog_id> --node <host>`
/// via the defer queue (the same cross-node pattern as download-batch).
async fn do_load(
    pg: &PgPool,
    host: &str,
    runtime: &str,
    library_id: &str,
    catalog_id: &str,
) -> Result<bool, String> {
    let this = crate::fleet_info::resolve_this_worker_name().await;
    if host.eq_ignore_ascii_case(&this) {
        let port = crate::ports_registry::pick_llm_port(pg, host, runtime)
            .await
            .map(|p| p as u16)
            .unwrap_or(55000);
        model_runtime::load_model(
            pg,
            LoadOptions {
                library_id: library_id.to_string(),
                port,
                context_size: None,
                parallel: None,
                agent_profile: true, // agent-capable serving profile.
                mmproj_path: None,   // auto-detect sibling mmproj
            },
        )
        .await
        .map(|res| {
            info!(host = %host, port = res.port, deployment = %res.deployment_id, "autoscaler: loaded model locally");
            true
        })
    } else {
        // Cross-node: enqueue an `ff model autoload --node` defer-shell task —
        // but FIRST guard against re-enqueuing. The autoscaler re-decides every
        // tick (~6min); without this guard a model that is still loading (large
        // gguf, minutes) OR that keeps failing gets a FRESH task every tick. A
        // broken autoload (e.g. the pre-#563 'resolve gguf' bug) flooded the
        // deferred queue with ~10 identical tasks/hour — the same retry-forever
        // pattern behind the 748/248-per-day failure floods. Skip if an autoload
        // for this (host, model) is already in-flight, or if the last one FAILED
        // within the cooldown window.
        let title = format!("autoscaler: autoload {catalog_id} on {host}");
        if let Some(reason) = autoscaler_enqueue_block_reason(pg, &title, host).await {
            debug!(host = %host, %catalog_id, reason, "autoscaler: skipping cross-node autoload");
            return Ok(false);
        }
        let command = format!(
            "~/.local/bin/ff model autoload {} --node {} --agent",
            shell_quote(catalog_id),
            shell_quote(host)
        );
        let payload = serde_json::json!({ "command": command });
        let trigger_spec = serde_json::json!({});
        ff_db::pg_enqueue_deferred(
            pg,
            &title,
            "shell",
            &payload,
            "now",
            &trigger_spec,
            Some(host),
            &serde_json::json!([]),
            Some(RESERVE_OWNER),
            Some(3),
        )
        .await
        .map(|defer_id| {
            info!(host = %host, %catalog_id, %defer_id, "autoscaler: enqueued cross-node load");
            true
        })
        .map_err(|e| format!("pg_enqueue_deferred(load on {host}): {e}"))
    }
}

/// After a cross-node autoload fails, wait this long before the autoscaler is
/// allowed to re-enqueue the SAME (host, model) load. Long enough to break a
/// tight retry-flood (tick is ~6min) yet short enough that a transient failure
/// (brief OOM, host blip) recovers within a few ticks.
const AUTOSCALER_ENQUEUE_COOLDOWN_SECS: i64 = 30 * 60;

/// Return `Some(reason)` if the autoscaler should NOT enqueue a fresh cross-node
/// task with this exact `title` targeting `host`. Used for BOTH autoload and
/// reprofile — each re-decides every tick and would otherwise pile duplicate /
/// retry-flood tasks into the deferred queue (the #564 pattern). Looks at the
/// MOST RECENT prior task with this exact (title, host) and defers to the pure
/// [`autoscaler_skip_reason`] policy. Best-effort: on a DB error we do NOT block
/// (returns None) so a transient query failure can't wedge autoscaling.
async fn autoscaler_enqueue_block_reason(
    pg: &PgPool,
    title: &str,
    host: &str,
) -> Option<&'static str> {
    let recent = ff_db::pg_list_deferred(pg, None, 300).await.ok()?;
    let prior = recent
        .iter()
        .find(|t| t.title == title && t.preferred_node.as_deref() == Some(host))?;
    let age_secs = (chrono::Utc::now() - prior.created_at).num_seconds();
    autoscaler_skip_reason(&prior.status, age_secs, AUTOSCALER_ENQUEUE_COOLDOWN_SECS)
}

/// Pure re-enqueue policy given the most recent prior task's `status` and
/// `age_secs`. In-flight states always block (don't pile on work that is still
/// running); a `failed` task blocks only inside the cooldown window; anything
/// else (completed/cancelled/none) permits a fresh enqueue.
fn autoscaler_skip_reason(status: &str, age_secs: i64, cooldown_secs: i64) -> Option<&'static str> {
    match status {
        "pending" | "dispatchable" | "running" | "claimed" => Some("already in-flight"),
        "failed" if age_secs < cooldown_secs => Some("recent failure (cooldown)"),
        _ => None,
    }
}

/// Perform an UNLOAD: locally via `model_runtime::unload_model` when the host is
/// this node, else `ff model unload <deployment_id> --node <host>` over SSH.
async fn do_unload(pg: &PgPool, host: &str, deployment_id: &str) -> Result<bool, String> {
    let this = crate::fleet_info::resolve_this_worker_name().await;
    if host.eq_ignore_ascii_case(&this) {
        model_runtime::unload_model(pg, deployment_id).await.map(|_| {
            info!(host = %host, deployment = %deployment_id, "autoscaler: unloaded model locally");
            true
        })
    } else {
        let node = ff_db::pg_get_node(pg, host)
            .await
            .map_err(|e| format!("pg_get_node({host}): {e}"))?
            .ok_or_else(|| format!("node '{host}' not in fleet_workers"))?;
        let remote_cmd = format!(
            "~/.local/bin/ff model unload {}",
            shell_quote(deployment_id)
        );
        let (code, _out, err) =
            crate::model_transfer::ssh_exec(&node.ssh_user, &node.ip, &remote_cmd)
                .await
                .map_err(|e| format!("ssh {host}: {e}"))?;
        if code != 0 {
            return Err(format!(
                "remote unload on {host} exited {code}: {}",
                err.trim()
            ));
        }
        info!(host = %host, deployment = %deployment_id, "autoscaler: unloaded model cross-node");
        Ok(true)
    }
}

/// Perform a REPROFILE: enqueue an `ff model reprofile <deployment_id>` defer-shell
/// task targeted at the owning host. We dispatch via the defer queue (NOT inline)
/// because `ff model reprofile` health-waits the relaunch up to 90s — far too long
/// to block a tick. The `ff model reprofile` primitive carries ALL the safety
/// (refuses non-tool-calling, no-ops if already agent-ready, RAM-headroom check,
/// loud failure on a down-window), so the autoscaler only has to pick an idle
/// candidate and hand it off — mirrors `do_load`'s cross-node pattern.
async fn do_reprofile(pg: &PgPool, host: &str, deployment_id: &str) -> Result<bool, String> {
    let command = format!(
        "~/.local/bin/ff model reprofile {}",
        shell_quote(deployment_id)
    );
    let payload = serde_json::json!({ "command": command });
    let trigger_spec = serde_json::json!({});
    let title = format!("autoscaler: reprofile {deployment_id} on {host}");
    // Same dedup/cooldown guard as do_load (#564): the autoscaler re-decides
    // every tick, so without this a reprofile that is still running (ff model
    // reprofile health-waits the relaunch up to 90s) or that keeps failing would
    // get a fresh task every ~6min and flood the deferred queue.
    if let Some(reason) = autoscaler_enqueue_block_reason(pg, &title, host).await {
        debug!(host = %host, %deployment_id, reason, "autoscaler: skipping reprofile enqueue");
        return Ok(false);
    }
    ff_db::pg_enqueue_deferred(
        pg,
        &title,
        "shell",
        &payload,
        "now",
        &trigger_spec,
        Some(host),
        &serde_json::json!([]),
        Some(RESERVE_OWNER),
        Some(3),
    )
    .await
    .map(|defer_id| {
        info!(host = %host, %deployment_id, %defer_id, "autoscaler: enqueued reprofile");
        true
    })
    .map_err(|e| format!("pg_enqueue_deferred(reprofile on {host}): {e}"))
}

/// Minimal single-quote shell escaping for the defer-shell / SSH command we build.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One autoscaler pass. Reads the gate; off = no-op. Plans the load/unload, logs
/// the decision, and (only in `active`) actuates. Returns the pass summary.
pub async fn autoscale_pass(pg: &PgPool) -> Result<AutoscaleSummary, String> {
    let mode = read_mode(pg).await;
    if mode == AutoscalerMode::Off {
        debug!("autoscaler: mode=off (no-op)");
        return Ok(AutoscaleSummary {
            mode: "off",
            ..Default::default()
        });
    }

    // Clear any reservations we orphaned on a prior crash/failover before planning.
    if let Err(e) = ff_db::pg_reap_stale_reservations(pg, RESERVE_OWNER, RESERVATION_TTL_SECS).await
    {
        warn!(error = %e, "autoscaler: stale-reservation reaper failed");
    }

    let (actions, mut summary) = plan_pass(pg).await?;
    summary.mode = mode.as_str();

    // Always log the decision with the demand/supply numbers + the plan.
    for action in &actions {
        match action {
            Action::Load {
                kind,
                host,
                catalog_id,
                size_gb,
                score,
                wanted,
                supplied,
                ..
            } => {
                info!(
                    kind = kind.label(),
                    %host,
                    %catalog_id,
                    size_gb = format!("{size_gb:.1}"),
                    score = format!("{score:.2}"),
                    wanted = format!("{wanted:.2}"),
                    supplied,
                    mode = mode.as_str(),
                    "autoscaler PLAN: would load {} on {} because {} target={:.2} supply={}",
                    catalog_id, host, kind.label(), wanted, supplied
                );
            }
            Action::Unload {
                kind,
                host,
                deployment_id,
                wanted,
                supplied,
            } => {
                info!(
                    kind = kind.label(),
                    %host,
                    %deployment_id,
                    wanted = format!("{wanted:.2}"),
                    supplied,
                    mode = mode.as_str(),
                    "autoscaler PLAN: would unload {} on {} because {} oversupplied target={:.2} supply={}",
                    deployment_id, host, kind.label(), wanted, supplied
                );
            }
            Action::Reprofile {
                kind,
                host,
                deployment_id,
                catalog_id,
                wanted,
                supplied,
            } => {
                info!(
                    kind = kind.label(),
                    %host,
                    %deployment_id,
                    %catalog_id,
                    wanted = format!("{wanted:.2}"),
                    supplied,
                    mode = mode.as_str(),
                    "autoscaler PLAN: would reprofile {} ({}) on {} into the agent profile because {} undersupplied (no fresh load fit) target={:.2} supply={}",
                    deployment_id, catalog_id, host, kind.label(), wanted, supplied
                );
            }
        }
    }

    // dry-run: stop here — actuate nothing.
    if mode == AutoscalerMode::DryRun {
        return Ok(summary);
    }

    // active: actuate.
    for action in &actions {
        match actuate(pg, action).await {
            Ok(true) => match action {
                Action::Load { kind, .. } => {
                    summary.loaded += 1;
                    dwell_mark(*kind);
                }
                Action::Unload { kind, .. } => {
                    summary.unloaded += 1;
                    dwell_mark(*kind);
                }
                Action::Reprofile { kind, .. } => {
                    summary.reprofiled += 1;
                    dwell_mark(*kind);
                }
            },
            Ok(false) => { /* reservation lost — host owned by someone else this pass */ }
            Err(e) => warn!(error = %e, "autoscaler: action failed"),
        }
    }

    Ok(summary)
}

/// Spawn the leader-gated autoscaler loop. The leader gate is read from the
/// process-local leader cache; the serving mix is global state, so only the
/// leader plans/actuates (no N-way races).
pub fn spawn_autoscaler_tick(
    pg: PgPool,
    _worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the immediate fire so pulse/election settle first.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !crate::leader_cache::is_current_leader() {
                        continue;
                    }

                    match autoscale_pass(&pg).await {
                        Ok(s) => {
                            // Always emit a one-line decision summary on the leader.
                            info!(
                                mode = s.mode,
                                code_wanted = format!("{:.2}", s.code_wanted),
                                code_anticipated = format!("{:.2}", s.code_anticipated),
                                code_target = format!("{:.2}", s.code_target),
                                code_supplied = s.code_supplied,
                                general_wanted = format!("{:.2}", s.general_wanted),
                                general_anticipated = format!("{:.2}", s.general_anticipated),
                                general_target = format!("{:.2}", s.general_target),
                                general_supplied = s.general_supplied,
                                leader_excluded = s.leader_excluded,
                                planned_loads = s.planned_loads,
                                planned_unloads = s.planned_unloads,
                                planned_reprofiles = s.planned_reprofiles,
                                loaded = s.loaded,
                                unloaded = s.unloaded,
                                reprofiled = s.reprofiled,
                                skipped_dwell = s.skipped_dwell,
                                no_fit = s.no_fit,
                                "autoscaler pass"
                            );
                        }
                        Err(e) => warn!(error = %e, "autoscaler tick failed"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("autoscaler tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoload_skip_inflight_states_always_block() {
        // A still-loading large model must not get a duplicate task piled on.
        for s in ["pending", "dispatchable", "running", "claimed"] {
            assert!(
                autoscaler_skip_reason(s, 999_999, AUTOSCALER_ENQUEUE_COOLDOWN_SECS).is_some(),
                "{s} should block regardless of age"
            );
        }
    }

    #[test]
    fn autoload_skip_failed_respects_cooldown() {
        let cd = AUTOSCALER_ENQUEUE_COOLDOWN_SECS;
        // Just failed → block (cooldown).
        assert!(autoscaler_skip_reason("failed", 60, cd).is_some());
        // Failed long ago → allow a retry.
        assert!(autoscaler_skip_reason("failed", cd + 1, cd).is_none());
    }

    #[test]
    fn autoload_skip_terminal_success_permits_reenqueue() {
        // A model that loaded then got unloaded must be reloadable immediately.
        assert!(autoscaler_skip_reason("completed", 1, AUTOSCALER_ENQUEUE_COOLDOWN_SECS).is_none());
        assert!(autoscaler_skip_reason("cancelled", 1, AUTOSCALER_ENQUEUE_COOLDOWN_SECS).is_none());
    }

    fn host(
        name: &str,
        status: &str,
        os: &str,
        gpu: &str,
        vram: Option<f64>,
        ram: i32,
        resident: f64,
        reservation: &str,
        active: i64,
    ) -> ff_db::PlacementCandidate {
        ff_db::PlacementCandidate {
            worker_name: name.into(),
            primary_ip: "10.0.0.1".into(),
            os_family: os.into(),
            gpu_kind: Some(gpu.into()),
            has_gpu: gpu != "none",
            gpu_total_vram_gb: vram,
            total_ram_gb: Some(ram),
            reservation_state: reservation.into(),
            status: status.into(),
            active_deployments: active,
            resident_model_gb: resident,
            free_ram_gb: ram as f64 - resident,
        }
    }

    fn rcand(
        id: &str,
        worker: &str,
        is_code: bool,
        request_count: i64,
        health_age_sec: Option<i32>,
    ) -> ff_db::ReprofileCandidate {
        ff_db::ReprofileCandidate {
            deployment_id: id.into(),
            worker_name: worker.into(),
            port: 55000,
            catalog_id: Some("qwen36-35b-a3b".into()),
            runtime: "llama.cpp".into(),
            usable_agent_ctx: Some(8192),
            parallel_slots: Some(4),
            request_count,
            health_age_sec,
            is_code,
        }
    }

    #[test]
    fn reprofile_pick_matches_kind_and_skips_busy_excluded_leader() {
        let cands = vec![
            // leader-hosted: excluded even though idle + right kind.
            rcand("leader-ep", "logan", true, 0, Some(9999)),
            // EXCLUDE_HOSTS (taylor): excluded.
            rcand("excl-ep", "taylor", true, 0, Some(9999)),
            // busy (in-flight requests + fresh health): not idle → skipped.
            rcand("busy-ep", "veronica", true, 5, Some(10)),
            // wrong kind for a code request.
            rcand("gen-ep", "james", false, 0, Some(9999)),
            // the valid pick: code, non-leader, non-excluded, idle (stale health).
            rcand("good-ep", "lily", true, 3, Some(IDLE_HEALTH_AGE_SECS + 1)),
        ];
        let pick = pick_reprofile_candidate(&cands, true, Some("logan"));
        assert_eq!(pick.map(|c| c.deployment_id.as_str()), Some("good-ep"));

        // No code candidate qualifies once the only idle one is the leader's.
        let only_leader = vec![rcand("leader-ep", "logan", true, 0, Some(9999))];
        assert!(pick_reprofile_candidate(&only_leader, true, Some("logan")).is_none());

        // request_count==0 also counts as idle even with a fresh health ping.
        let zero_req = vec![rcand("zero", "duncan", true, 0, Some(1))];
        assert_eq!(
            pick_reprofile_candidate(&zero_req, true, Some("logan"))
                .map(|c| c.deployment_id.as_str()),
            Some("zero")
        );
    }

    #[test]
    fn reprofile_pick_prefers_most_idle() {
        let cands = vec![
            rcand("a", "lily", true, 1, Some(IDLE_HEALTH_AGE_SECS + 10)),
            rcand("b", "veronica", true, 1, Some(IDLE_HEALTH_AGE_SECS + 500)),
        ];
        // Most idle (oldest health ping) wins.
        assert_eq!(
            pick_reprofile_candidate(&cands, true, None).map(|c| c.deployment_id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn scaleup_count_includes_load_and_reprofile() {
        let load = Action::Load {
            kind: Kind::Code,
            host: "lily".into(),
            library_id: "lib".into(),
            catalog_id: "m".into(),
            runtime: "llama.cpp".into(),
            size_gb: 20.0,
            score: 1.0,
            wanted: 2.0,
            supplied: 1,
        };
        let reprofile = Action::Reprofile {
            kind: Kind::Code,
            host: "veronica".into(),
            deployment_id: "dep".into(),
            catalog_id: "m".into(),
            wanted: 2.0,
            supplied: 1,
        };
        let unload = Action::Unload {
            kind: Kind::General,
            host: "james".into(),
            deployment_id: "dep2".into(),
            wanted: 0.0,
            supplied: 2,
        };
        use std::slice::from_ref;
        assert_eq!(scaleup_action_count(&[]), 0);
        assert_eq!(scaleup_action_count(from_ref(&unload)), 0);
        assert_eq!(scaleup_action_count(from_ref(&load)), 1);
        assert_eq!(scaleup_action_count(from_ref(&reprofile)), 1);
        assert_eq!(scaleup_action_count(&[load, reprofile, unload]), 2);
    }

    #[test]
    fn mode_parsing_defaults_off() {
        assert_eq!(AutoscalerMode::parse(None), AutoscalerMode::Off);
        assert_eq!(AutoscalerMode::parse(Some("")), AutoscalerMode::Off);
        assert_eq!(AutoscalerMode::parse(Some("garbage")), AutoscalerMode::Off);
        assert_eq!(AutoscalerMode::parse(Some("off")), AutoscalerMode::Off);
        assert_eq!(
            AutoscalerMode::parse(Some("dry-run")),
            AutoscalerMode::DryRun
        );
        assert_eq!(
            AutoscalerMode::parse(Some("DRY_RUN")),
            AutoscalerMode::DryRun
        );
        assert_eq!(
            AutoscalerMode::parse(Some(" active ")),
            AutoscalerMode::Active
        );
    }

    #[test]
    fn floor_parsing_clamps_and_defaults() {
        // Missing / empty / garbage → default.
        assert_eq!(parse_floor(None, 2.0), 2.0);
        assert_eq!(parse_floor(Some("  "), 2.0), 2.0);
        assert_eq!(parse_floor(Some("nope"), 2.0), 2.0);
        assert_eq!(parse_floor(Some("NaN"), 2.0), 2.0);
        assert_eq!(parse_floor(Some("inf"), 2.0), 2.0);
        // Valid values pass through, trimmed.
        assert_eq!(parse_floor(Some("1"), 2.0), 1.0);
        assert_eq!(parse_floor(Some(" 3 "), 2.0), 3.0);
        assert_eq!(parse_floor(Some("0"), 2.0), 0.0);
        // Out-of-range clamps into [0, MAX_FLOOR].
        assert_eq!(parse_floor(Some("-5"), 2.0), 0.0);
        assert_eq!(parse_floor(Some("9999"), 2.0), MAX_FLOOR);
    }

    #[test]
    fn slope_detects_direction() {
        // Rising series → positive slope.
        assert!(least_squares_slope(&[0.0, 1.0, 2.0, 3.0]) > 0.0);
        // Falling → negative.
        assert!(least_squares_slope(&[3.0, 2.0, 1.0, 0.0]) < 0.0);
        // Flat → ~0.
        assert!(least_squares_slope(&[2.0, 2.0, 2.0]).abs() < 1e-9);
        // Degenerate inputs → 0 (no trend).
        assert_eq!(least_squares_slope(&[]), 0.0);
        assert_eq!(least_squares_slope(&[5.0]), 0.0);
        // Exact unit slope over evenly spaced points.
        assert!((least_squares_slope(&[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn anticipate_only_pre_warms_on_rising_trend() {
        // Empty → 0.
        assert_eq!(anticipate(&[]), 0.0);
        // Single sample → that value (no trend to project).
        assert_eq!(anticipate(&[1.5]), 1.5);
        // Flat → current unchanged.
        assert_eq!(anticipate(&[2.0, 2.0, 2.0]), 2.0);
        // Falling → current unchanged (never pre-warm downward).
        assert_eq!(anticipate(&[3.0, 2.0, 1.0]), 1.0);
        // Rising slope=1, horizon=1 → current(2)+1 = 3, within the +2 cap.
        let a = anticipate(&[0.0, 1.0, 2.0]);
        assert!((a - 3.0).abs() < 1e-9, "expected ~3.0, got {a}");
        // Steep ramp is clamped to current + MAX_ANTICIPATION_SLOTS.
        let steep = anticipate(&[0.0, 5.0, 10.0]);
        assert!(
            (steep - (10.0 + MAX_ANTICIPATION_SLOTS)).abs() < 1e-9,
            "steep ramp should clamp to current+{MAX_ANTICIPATION_SLOTS}, got {steep}"
        );
        // Result is never below current on any input.
        assert!(anticipate(&[1.0, 4.0, 2.0]) >= 2.0);
    }

    // Authored by a fleet model (qwen36 on lily) via `ff offload`, then verified
    // by hand and integrated — dogfooding the fleet for test-gen. Pins the
    // model-placement RAM working-set formula (size*1.15 overhead + a constant
    // 2GB KV cache for the 32K AGENT_MIN_CTX slot) so a careless edit can't
    // silently change placement RAM accounting.
    #[test]
    fn agent_working_set_gb_formula() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // size_gb = 0.0 -> 0.0 * 1.15 + 2.0 = 2.0
        assert!(approx(agent_working_set_gb(0.0), 2.0));
        // size_gb = 10.0 -> 11.5 + 2.0 = 13.5
        assert!(approx(agent_working_set_gb(10.0), 13.5));
        // size_gb = 20.0 -> 23.0 + 2.0 = 25.0
        assert!(approx(agent_working_set_gb(20.0), 25.0));
        // size_gb = 100.0 -> 115.0 + 2.0 = 117.0
        assert!(approx(agent_working_set_gb(100.0), 117.0));
    }

    #[test]
    fn rank_deficits_orders_and_filters() {
        // Below-deadband kinds are dropped; remaining sorted most-starved first.
        let ranked = rank_deficits(&[(Kind::Code, 1.2, 1), (Kind::General, 3.0, 0)]);
        assert_eq!(ranked.len(), 1, "code deficit 0.2 is below the deadband");
        assert_eq!(ranked[0].0, Kind::General);

        // Two over-deadband kinds: the larger deficit comes first so a feasible
        // fall-through tries the most-starved kind before the runner-up.
        let ranked = rank_deficits(&[(Kind::Code, 2.0, 0), (Kind::General, 5.0, 0)]);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0, Kind::General); // deficit 5.0 > 2.0
        assert_eq!(ranked[1].0, Kind::Code);

        // At/under the floor for both → no candidates (no churn at demand=0).
        let ranked = rank_deficits(&[(Kind::Code, 2.0, 2), (Kind::General, 0.0, 0)]);
        assert!(ranked.is_empty());
    }

    fn endpoint(worker: &str) -> ff_db::ServingEndpoint {
        ff_db::ServingEndpoint {
            deployment_id: format!("dep-{worker}"),
            worker_name: worker.to_string(),
            port: 55000,
            catalog_id: Some("qwen36-35b-a3b".into()),
            request_count: 0,
            health_age_sec: Some(1),
        }
    }

    #[test]
    fn exclude_leader_supply_drops_only_leader_endpoints() {
        // The classic P0 shape: floor=2 is "met" by taylor(leader) + logan, but
        // the swarm soft-excludes the leader, so only logan is usable. After
        // excluding the leader, code supply reads 1 → the floor will warm a 2nd
        // NON-LEADER endpoint.
        let supply = ff_db::ServingSupply {
            code_count: 2,
            general_count: 1,
            code_endpoints: vec![endpoint("taylor"), endpoint("logan")],
            general_endpoints: vec![endpoint("taylor")],
        };
        let (filtered, excluded) = exclude_leader_supply(supply, Some("taylor"));
        assert_eq!(excluded, 2, "both taylor-hosted endpoints dropped");
        assert_eq!(filtered.code_count, 1);
        assert_eq!(filtered.general_count, 0);
        assert_eq!(filtered.code_endpoints.len(), 1);
        assert_eq!(filtered.code_endpoints[0].worker_name, "logan");
        assert!(filtered.general_endpoints.is_empty());
    }

    #[test]
    fn exclude_leader_supply_fails_open_without_leader() {
        // Leader unknown (DB read failed) → count everything, the pre-leader-aware
        // behavior. Never zero out supply just because we couldn't resolve a leader.
        let supply = ff_db::ServingSupply {
            code_count: 2,
            general_count: 0,
            code_endpoints: vec![endpoint("taylor"), endpoint("logan")],
            general_endpoints: vec![],
        };
        let (filtered, excluded) = exclude_leader_supply(supply, None);
        assert_eq!(excluded, 0);
        assert_eq!(filtered.code_count, 2);
    }

    #[test]
    fn exclude_leader_supply_noop_when_leader_serves_nothing() {
        // Leader hosts no agent endpoint (the healthy steady state we want) →
        // nothing is dropped, counts unchanged.
        let supply = ff_db::ServingSupply {
            code_count: 2,
            general_count: 0,
            code_endpoints: vec![endpoint("logan"), endpoint("lily")],
            general_endpoints: vec![],
        };
        let (filtered, excluded) = exclude_leader_supply(supply, Some("taylor"));
        assert_eq!(excluded, 0);
        assert_eq!(filtered.code_count, 2);
        assert_eq!(filtered.code_endpoints.len(), 2);
    }

    #[test]
    fn oom_guard_rejects_when_no_headroom() {
        // 31GB host already serving a 20GB model; a 20GB working set won't fit.
        let h = host(
            "marcus",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            31,
            20.0,
            "available",
            1,
        );
        assert_eq!(score_host(&h, "llama.cpp", 20.0), None);
    }

    #[test]
    fn memory_tight_host_is_never_used() {
        if std::env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && std::env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            return;
        }
        // 16GB host — at/under the build-free threshold — is gated out entirely.
        let h = host(
            "ace",
            "online",
            "macos",
            "apple_silicon",
            None,
            16,
            0.0,
            "available",
            0,
        );
        assert_eq!(score_host(&h, "mlx", 5.0), None);
    }

    #[test]
    fn reserved_or_drained_host_is_ineligible() {
        let h = host(
            "sophie",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            96,
            0.0,
            "reserved",
            0,
        );
        assert_eq!(score_host(&h, "llama.cpp", 20.0), None);
        let h2 = host(
            "sophie",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            96,
            0.0,
            "drained",
            0,
        );
        assert_eq!(score_host(&h2, "llama.cpp", 20.0), None);
    }

    #[test]
    fn excluded_leader_is_ineligible() {
        let h = host(
            "taylor",
            "online",
            "macos",
            "apple_silicon",
            None,
            96,
            0.0,
            "available",
            0,
        );
        assert_eq!(score_host(&h, "mlx", 20.0), None);
    }

    #[test]
    fn amd_rocm_gtt_unified_uses_full_ram_pool() {
        // EVO-X2: 123GB RAM but only 2.1GB discrete VRAM → GTT-unified, the whole
        // RAM is the pool, so a 20GB model fits (would be rejected if scored on
        // the 2.1GB discrete number).
        let h = host(
            "logan",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            123,
            0.0,
            "available",
            0,
        );
        assert!(score_host(&h, "llama.cpp", 25.0).is_some());
    }

    #[test]
    fn runtime_mismatch_is_ineligible() {
        // mlx model can't run on a Linux host.
        let linux = host(
            "sophie",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            96,
            0.0,
            "available",
            0,
        );
        assert_eq!(score_host(&linux, "mlx", 10.0), None);
        // vllm needs CUDA/GB10; an apple_silicon host is ineligible.
        let mac = host(
            "james",
            "online",
            "macos",
            "apple_silicon",
            None,
            63,
            0.0,
            "available",
            0,
        );
        assert_eq!(score_host(&mac, "vllm", 10.0), None);
    }

    #[test]
    fn least_loaded_host_scores_higher() {
        let idle = host(
            "sophie",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            96,
            0.0,
            "available",
            0,
        );
        let busy = host(
            "priya",
            "online",
            "linux-ubuntu",
            "amd_rocm",
            Some(2.1),
            96,
            0.0,
            "available",
            5,
        );
        let s_idle = score_host(&idle, "llama.cpp", 20.0).unwrap();
        let s_busy = score_host(&busy, "llama.cpp", 20.0).unwrap();
        assert!(s_idle > s_busy, "idle {s_idle} should beat busy {s_busy}");
    }
}
