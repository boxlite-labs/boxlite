//! `boxlite serve` — long-running REST API server.
//!
//! Holds a single BoxliteRuntime and exposes the full REST API
//! over HTTP so that `Boxlite.rest()` clients can connect.

mod handlers;
mod types;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use clap::Args;
use futures::StreamExt;
use tokio::sync::RwLock;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

use boxlite::runtime::options::{InboundNetworkConfig, NetworkMode, OutboundNetworkConfig};
use boxlite::{
    BoxCommand, BoxInfo, BoxOptions, BoxStatus, BoxliteRuntime, ExecStdin, Execution, LiteBox,
    NetworkSpec, RootfsSpec,
};

use crate::cli::GlobalFlags;
use crate::defaults::{LOCAL_SERVE_HOST, LOCAL_SERVE_PORT};

use self::types::{BoxResponse, CreateBoxRequest, ErrorBody, ErrorDetail, ExecRequest};

fn parse_api_key(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        Err("API key must not be empty".to_string())
    } else {
        Ok(raw.to_string())
    }
}

// ============================================================================
// CLI Args
// ============================================================================

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Port to listen on. Defaults to `LOCAL_SERVE_PORT`.
    #[arg(long, default_value_t = LOCAL_SERVE_PORT)]
    pub port: u16,

    /// Host/address to bind to. Defaults to `LOCAL_SERVE_HOST`.
    #[arg(long, default_value_t = LOCAL_SERVE_HOST.to_string())]
    pub host: String,

    /// Optional expected API key. When set, every route except
    /// `GET /v1/config` requires `Authorization: Bearer <this>` (constant-time
    /// match) and returns 401 otherwise. Unset = permissive (accepts any/no
    /// bearer) — the zero-config local-dev default.
    #[arg(
        long,
        env = "BOXLITE_SERVE_API_KEY",
        hide_env_values = true,
        value_parser = parse_api_key
    )]
    pub api_key: Option<String>,
}

// ============================================================================
// Shared State
// ============================================================================

struct AppState {
    runtime: BoxliteRuntime,
    /// Cached box handles (box_id -> Arc<LiteBox>).
    boxes: RwLock<HashMap<String, Arc<LiteBox>>>,
    /// Active executions (execution_id -> ActiveExecution). Holds an
    /// `Arc` so attach sessions can drop the map lock before doing
    /// long-running WS pumping while keeping the exec alive.
    executions: RwLock<HashMap<String, Arc<ActiveExecution>>>,
    /// Optional expected API key (`--api-key` / `$BOXLITE_SERVE_API_KEY`).
    /// `None` ⇒ permissive (no auth enforced).
    api_key: Option<String>,
    /// Lifecycle deadlines per box, in seconds.
    ///
    /// Held here rather than in `BoxOptions` because the engine accepts neither
    /// on a local runtime, which is what `serve` drives:
    ///
    /// - `auto_stop` is refused outright — `reject_local_unsupported_options`
    ///   (`runtime/rt_impl.rs:1743`) returns Unsupported, because a local
    ///   runtime has no sweeper of its own.
    /// - `auto_delete` is accepted but means something else: `removes_on_stop()`
    ///   is `effective_auto_delete() > 0`, so passing a deadline through makes
    ///   the engine delete the box the instant it stops — the opposite of
    ///   waiting — and forces it non-detached.
    ///
    /// `serve` is the sweeper, so it keeps the policy and acts on it.
    ///
    /// In memory only: a restart forgets every deadline, and a box created
    /// before it is never swept.
    lifecycle: RwLock<HashMap<String, LifecyclePolicy>>,
    /// Last observed user activity per box, for AutoStop.
    ///
    /// `serve` holds the `$BOXLITE_HOME` lock for its whole life, so nothing
    /// else can touch these boxes while it runs and this map is a complete
    /// record. A box it has not seen is seeded at the current tick rather than
    /// measured against `BoxInfo.last_updated`, which tracks state transitions
    /// and would report a long-running busy box as idle since boot.
    last_activity: RwLock<HashMap<String, Instant>>,
}

/// The deadlines `serve` enforces for one box. `0` disables either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::commands::serve) struct LifecyclePolicy {
    pub auto_stop: u32,
    pub auto_delete: u32,
}

impl LifecyclePolicy {
    fn is_empty(&self) -> bool {
        self.auto_stop == 0 && self.auto_delete == 0
    }
}

impl AppState {
    /// Remember a box's deadlines. A policy with neither set is not stored.
    pub(in crate::commands::serve) async fn set_lifecycle(
        &self,
        box_id: &str,
        policy: LifecyclePolicy,
    ) {
        if policy.is_empty() {
            return;
        }
        self.lifecycle
            .write()
            .await
            .insert(box_id.to_string(), policy);
    }

    /// The box's deadlines, or an all-zero policy when it has none.
    pub(in crate::commands::serve) async fn lifecycle_of(&self, box_id: &str) -> LifecyclePolicy {
        self.lifecycle
            .read()
            .await
            .get(box_id)
            .copied()
            .unwrap_or_default()
    }

    /// Render a box for the wire, with the deadlines this server holds.
    ///
    /// The single place a `BoxResponse` is built, because `BoxInfo` cannot
    /// carry these two fields here: `build_box_options` withholds both from the
    /// runtime, so `BoxInfo` reports the zeroes it was handed. Reading them off
    /// `info` would answer with no deadline on a box the sweep is enforcing one
    /// for. A box with no stored policy reports none, so a clone or an import
    /// does not inherit the deadline of the box it came from.
    pub(in crate::commands::serve) async fn box_response(&self, info: &BoxInfo) -> BoxResponse {
        let mut resp = box_info_to_response(info);
        // `auto_resume` is not overlaid: it *is* forwarded to the runtime, so
        // `info` already carries the caller's value back.
        let policy = self.lifecycle_of(info.id.as_ref()).await;
        resp.auto_stop = policy.auto_stop;
        resp.auto_delete = policy.auto_delete;
        resp
    }

    /// Mark a box as used right now, resetting its AutoStop window.
    ///
    /// The single write site for the idle clock, so the request middleware and
    /// the sweep agree on what "used" means.
    pub(in crate::commands::serve) async fn record_box_activity(&self, box_id: &str) {
        self.last_activity
            .write()
            .await
            .insert(box_id.to_string(), Instant::now());
    }

    /// Forget everything held about a box that is gone.
    ///
    /// Without this both maps grow for the life of the process, one entry per
    /// box ever deleted.
    pub(in crate::commands::serve) async fn forget_box(&self, box_id: &str) {
        self.lifecycle.write().await.remove(box_id);
        self.last_activity.write().await.remove(box_id);
    }

    /// Drop idle clocks for spellings that name no box.
    ///
    /// `forget_box` covers boxes this server deleted, but not ids that were
    /// never boxes: `record_activity` stamps the clock straight off the request
    /// path, before any handler can reject an unknown id, so a client looping
    /// over made-up ids would otherwise grow the map for the life of the
    /// process — and `serve` binds `0.0.0.0` and may run with no API key.
    ///
    /// Only `last_activity` is pruned. It is the map unauthenticated input can
    /// grow, and re-seeding a live box costs nothing but a fresh window. The
    /// `lifecycle` map is written only by `create_box`, always with a canonical
    /// id, so pruning it would buy nothing and would race: a box created after
    /// the caller's `list_info` snapshot would have its deadline dropped and
    /// never re-added.
    pub(in crate::commands::serve) async fn retain_known_boxes(
        &self,
        live: &[(String, Option<String>)],
    ) {
        self.last_activity
            .write()
            .await
            .retain(|spelling, _| box_named_by(spelling, live).is_some());
    }
}

/// Which stdio session an [`ActiveExecution`] fronts.
///
/// Both kinds live in the same `executions` registry, keyed by execution
/// id — but only an exec session can be *addressed* by that id: the main
/// session's id is the container id, which the guest assigns and `BoxInfo`
/// does not carry, so a client on `/boxes/{id}/attach` can only name the
/// box. Marking the kind at insert time is what lets `find_main_session`
/// recognize an already-open main session from the box id alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::commands::serve) enum SessionKind {
    /// A tenant exec — `POST /boxes/{box_id}/exec`.
    Exec,
    /// The box's main command session: the container's init (docker
    /// semantics — `run IMAGE COMMAND` runs COMMAND *as* init). The guest
    /// registers it under `execution_id == container_id`.
    Main,
}

/// Response header on the `/boxes/{box_id}/attach` upgrade carrying the
/// main session's execution id (the container id).
///
/// The client cannot know that id up front, but it needs one for every
/// *other* thing an `Execution` does — signal, resize, kill, status probe,
/// reconnect — all of which are addressed by execution id. Handing it back
/// on the 101 makes the main session an ordinary session from that point
/// on: no parallel control path, no second-class `Execution`. The client
/// half of this contract is `RestBox::attach` in
/// `src/boxlite/src/rest/litebox.rs`, which pins the same header name.
pub(in crate::commands::serve) const MAIN_SESSION_ID_HEADER: &str = "x-boxlite-execution-id";

/// Server-side state for one execution. The underlying `Execution`'s
/// stdout/stderr are consumed once at creation and tee'd into broadcast
/// channels so any number of attach sessions (over time) can subscribe.
/// The `Execution` itself is kept in the map so `wait()`, `kill()`,
/// `signal()`, `resize_tty()` and reattach all work.
pub(in crate::commands::serve) struct ActiveExecution {
    box_id: String,
    kind: SessionKind,
    execution: Execution,
    /// Stdin sink owned by the WS `/attach` session.
    stdin: tokio::sync::Mutex<Option<ExecStdin>>,
    /// Backlog-aware broadcast tees. Late subscribers see the backlog
    /// snapshot on subscribe, then live data — matching the Go runner's
    /// streamBus pattern.
    stdout_bus: Arc<BacklogBroadcast>,
    stderr_bus: Arc<BacklogBroadcast>,
    /// Single-attach + reaper state, all under one Mutex.
    attach: tokio::sync::Mutex<AttachState>,
    /// Whether the exec has been seen to complete (Done fired). Set by
    /// the wait task; checked by the reaper to skip already-exited execs.
    done: std::sync::atomic::AtomicBool,
    /// Watch-channel mirror of `done` for async observers. The wait task
    /// flips this to `true` after `Execution::wait()` returns; SSE and WS
    /// handlers `select!` on `done_rx.changed()` so they break out of their
    /// loops the instant the process completes (rather than waiting for the
    /// broadcast channel's receivers to see `Closed`, which they never do
    /// because `ActiveExecution` owns the master Senders for its lifetime).
    /// Pattern: Vector `RepairState` watch::channel<EnumState>
    /// (src/sinks/redis/sink.rs:130-135); ours is binary so bool suffices.
    done_tx: tokio::sync::watch::Sender<bool>,
    /// Final exit code, populated once Done fires. Read by the WS attach
    /// handler to send the `{"type":"exit", "exit_code":N}` text frame.
    exit_code: std::sync::atomic::AtomicI32,
    /// Stamped when the wait task fires. Used by the retention check so
    /// execs that ran longer than `COMPLETED_RETENTION_GRACE` are not
    /// evicted immediately on exit.
    done_at: std::sync::Mutex<Option<Instant>>,
    /// Used by the reaper to enforce the 24 h hard cap.
    created_at: Instant,
}

struct AttachState {
    connected: bool,
    /// Wall-clock instant when the single-attach slot last went idle.
    /// Initialized to the exec's creation time so a client that never
    /// calls `/attach` still escalates through SIGHUP→SIGTERM→SIGKILL
    /// at the reconnect_grace boundary. Cleared on successful
    /// `mark_connected()`, re-stamped on `mark_disconnected()`.
    last_disconnect_at: Option<Instant>,
    signaled_hup: bool,
    signaled_term: bool,
    /// Set by the reaper's final escalation (SIGKILL path). Once true,
    /// `mark_connected()` rejects so a late attach can't race the kill.
    reaping_kill: bool,
    /// True while the reaper is delivering a cooperative signal (HUP/TERM).
    /// `mark_connected()` rejects while set, closing the TOCTOU gap between
    /// `try_escalate_*` releasing the lock and `signal()` reaching the
    /// process. Cleared by `finish_escalation()` after delivery.
    escalating: bool,
}

/// Bounded buffer size for the stdout/stderr broadcast channels.
/// 256 chunks at ~4 KB each = ~1 MB of slack for a transiently slow
/// subscriber before it sees `RecvError::Lagged`.
const ATTACH_BROADCAST_CAPACITY: usize = 256;

/// Byte-capped backlog retained for replay on late (re)attach.
/// Matches the Go runner's `streamBusBacklogCap` (256 KiB).
const BACKLOG_BYTE_CAP: usize = 256 * 1024;

/// Broadcast sender with a bounded byte backlog for replay on subscribe.
///
/// Pattern mirrors Go runner's `streamBus` — `send()` appends to a
/// byte-capped backlog AND fans out via broadcast; `subscribe()` replays
/// the backlog snapshot then switches to live broadcast.
struct BacklogBroadcast {
    tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    state: std::sync::Mutex<BacklogState>,
    cap: usize,
}

struct BacklogState {
    backlog: std::collections::VecDeque<Vec<u8>>,
    total_bytes: usize,
}

impl BacklogBroadcast {
    fn new(capacity: usize, backlog_cap: usize) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(capacity);
        Self {
            tx,
            state: std::sync::Mutex::new(BacklogState {
                backlog: std::collections::VecDeque::new(),
                total_bytes: 0,
            }),
            cap: backlog_cap,
        }
    }

    fn send(&self, data: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.total_bytes += data.len();
        state.backlog.push_back(data.clone());
        // Always retain at least the most recent chunk so a late subscriber
        // sees something, even if a single chunk exceeds the byte cap.
        while state.total_bytes > self.cap && state.backlog.len() > 1 {
            if let Some(old) = state.backlog.pop_front() {
                state.total_bytes -= old.len();
            } else {
                break;
            }
        }
        // Broadcast under the same lock so subscribe() can't snapshot
        // the backlog AND receive the same chunk from the live channel.
        let _ = self.tx.send(data);
    }

    /// Subscribe with atomic backlog replay. The returned receiver
    /// yields the backlog snapshot first, then live broadcasts — no
    /// gap, no interleaving. Both the backlog snapshot and the
    /// broadcast subscribe happen under the state lock, which `send()`
    /// also holds through its `tx.send()`, preventing duplicates.
    fn subscribe(&self) -> BacklogReceiver {
        let state = self.state.lock().unwrap();
        let replay: std::collections::VecDeque<Vec<u8>> = state.backlog.iter().cloned().collect();
        let rx = self.tx.subscribe();
        BacklogReceiver { replay, rx }
    }
}

/// Receiver that yields backlog chunks first, then live broadcast.
/// Created by `BacklogBroadcast::subscribe()`.
struct BacklogReceiver {
    replay: std::collections::VecDeque<Vec<u8>>,
    rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
}

impl BacklogReceiver {
    async fn recv(&mut self) -> Result<Vec<u8>, tokio::sync::broadcast::error::RecvError> {
        if let Some(chunk) = self.replay.pop_front() {
            return Ok(chunk);
        }
        self.rx.recv().await
    }

    fn try_recv(&mut self) -> Result<Vec<u8>, tokio::sync::broadcast::error::TryRecvError> {
        if let Some(chunk) = self.replay.pop_front() {
            return Ok(chunk);
        }
        self.rx.try_recv()
    }
}

impl ActiveExecution {
    fn new(
        box_id: String,
        kind: SessionKind,
        mut execution: Execution,
        stdin: Option<ExecStdin>,
    ) -> Arc<Self> {
        let stdout = execution.stdout();
        let stderr = execution.stderr();

        let stdout_bus = Arc::new(BacklogBroadcast::new(
            ATTACH_BROADCAST_CAPACITY,
            BACKLOG_BYTE_CAP,
        ));
        let stderr_bus = Arc::new(BacklogBroadcast::new(
            ATTACH_BROADCAST_CAPACITY,
            BACKLOG_BYTE_CAP,
        ));
        let (done_tx, _) = tokio::sync::watch::channel(false);

        let now = Instant::now();
        let active = Arc::new(Self {
            box_id,
            kind,
            execution,
            stdin: tokio::sync::Mutex::new(stdin),
            stdout_bus: stdout_bus.clone(),
            stderr_bus: stderr_bus.clone(),
            attach: tokio::sync::Mutex::new(AttachState {
                connected: false,
                last_disconnect_at: Some(now),
                signaled_hup: false,
                signaled_term: false,
                reaping_kill: false,
                escalating: false,
            }),
            done: std::sync::atomic::AtomicBool::new(false),
            done_tx,
            exit_code: std::sync::atomic::AtomicI32::new(-1),
            done_at: std::sync::Mutex::new(None),
            created_at: now,
        });

        // Spawn pumps that read the (single-consumer) Stream half and
        // fan out via the backlog-aware broadcast. Unlike raw broadcast,
        // BacklogBroadcast retains recent output so late subscribers
        // see the backlog on subscribe.
        let stdout_handle = if let Some(mut out) = stdout {
            let bus = stdout_bus;
            Some(tokio::spawn(async move {
                while let Some(line) = out.next().await {
                    bus.send(line.into_bytes());
                }
            }))
        } else {
            None
        };
        let stderr_handle = if let Some(mut err) = stderr {
            let bus = stderr_bus;
            Some(tokio::spawn(async move {
                while let Some(line) = err.next().await {
                    bus.send(line.into_bytes());
                }
            }))
        } else {
            None
        };

        // Wait task: records exit code + flips done. Barriers the pump
        // tasks so all output is broadcast before done_tx fires.
        {
            let active = Arc::clone(&active);
            tokio::spawn(async move {
                if let Ok(result) = active.execution.wait().await {
                    active
                        .exit_code
                        .store(result.exit_code, std::sync::atomic::Ordering::SeqCst);
                }
                if let Some(h) = stdout_handle {
                    let _ = h.await;
                }
                if let Some(h) = stderr_handle {
                    let _ = h.await;
                }
                *active.done_at.lock().unwrap() = Some(Instant::now());
                active.done.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = active.done_tx.send(true);
            });
        }

        active
    }

    pub(in crate::commands::serve) fn box_id(&self) -> &str {
        &self.box_id
    }

    pub(in crate::commands::serve) fn kind(&self) -> SessionKind {
        self.kind
    }

    pub(in crate::commands::serve) fn stdout_bus(&self) -> &BacklogBroadcast {
        &self.stdout_bus
    }

    pub(in crate::commands::serve) fn stderr_bus(&self) -> &BacklogBroadcast {
        &self.stderr_bus
    }

    pub(in crate::commands::serve) fn stdin(&self) -> &tokio::sync::Mutex<Option<ExecStdin>> {
        &self.stdin
    }

    pub(in crate::commands::serve) fn execution(&self) -> &Execution {
        &self.execution
    }

    pub(in crate::commands::serve) fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Subscribe to the "process is done" watch channel. Callers select
    /// on `rx.changed()` to wake the instant the wait task fires.
    pub(in crate::commands::serve) fn done_rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.done_tx.subscribe()
    }

    pub(in crate::commands::serve) fn exit_code(&self) -> i32 {
        self.exit_code.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(in crate::commands::serve) fn created_at(&self) -> Instant {
        self.created_at
    }

    /// Whether a completed execution should still be retained in the map.
    /// Used by the reaper and tests.
    pub(in crate::commands::serve) fn should_retain(&self, now: Instant) -> bool {
        if !self.is_done() {
            return true;
        }
        let done_at = self.done_at.lock().unwrap();
        match *done_at {
            Some(at) => now.duration_since(at) <= COMPLETED_RETENTION_GRACE,
            None => true,
        }
    }

    /// Attempt to claim the single-attach slot. Returns true on success;
    /// false if another client is already attached OR the reaper has
    /// claimed a terminal kill. Resets escalation flags on success so a
    /// fresh disconnect starts a fresh reap clock.
    pub(in crate::commands::serve) async fn mark_connected(&self) -> bool {
        let mut s = self.attach.lock().await;
        if s.connected || s.reaping_kill || s.escalating {
            return false;
        }
        s.connected = true;
        s.last_disconnect_at = None;
        s.signaled_hup = false;
        s.signaled_term = false;
        true
    }

    pub(in crate::commands::serve) async fn mark_disconnected(&self) {
        let mut s = self.attach.lock().await;
        s.connected = false;
        s.last_disconnect_at = Some(Instant::now());
    }

    /// Set the terminal reaping flag so mark_connected() rejects.
    /// Used by the hard-cap kill path which bypasses the escalation
    /// state machine.
    pub(in crate::commands::serve) async fn mark_reaping_kill(&self) {
        let mut s = self.attach.lock().await;
        s.reaping_kill = true;
    }

    async fn is_reaping_kill(&self) -> bool {
        let s = self.attach.lock().await;
        s.reaping_kill
    }

    /// Reaper: atomically try to escalate to SIGHUP. Sets `escalating`
    /// to block concurrent `mark_connected()` during signal delivery.
    /// Returns `true` if the transition was taken; `false` if skipped.
    async fn try_escalate_hup(&self, now: Instant, reconnect_grace: std::time::Duration) -> bool {
        let mut s = self.attach.lock().await;
        if s.connected || s.signaled_hup || s.reaping_kill || s.escalating {
            return false;
        }
        let Some(disc) = s.last_disconnect_at else {
            return false;
        };
        if now.duration_since(disc) <= reconnect_grace {
            return false;
        }
        s.signaled_hup = true;
        s.escalating = true;
        s.last_disconnect_at = Some(now);
        true
    }

    /// Reaper: atomically try to escalate to SIGTERM.
    async fn try_escalate_term(&self, now: Instant, shutdown_grace: std::time::Duration) -> bool {
        let mut s = self.attach.lock().await;
        if s.connected || !s.signaled_hup || s.signaled_term || s.reaping_kill || s.escalating {
            return false;
        }
        let Some(disc) = s.last_disconnect_at else {
            return false;
        };
        if now.duration_since(disc) <= shutdown_grace {
            return false;
        }
        s.signaled_term = true;
        s.escalating = true;
        s.last_disconnect_at = Some(now);
        true
    }

    /// Reaper: atomically try to escalate to SIGKILL. Once this returns
    /// `true`, `mark_connected()` will reject — the exec is doomed.
    async fn try_escalate_kill(&self, now: Instant, shutdown_grace: std::time::Duration) -> bool {
        let mut s = self.attach.lock().await;
        if s.connected || !s.signaled_term || s.reaping_kill {
            return false;
        }
        let Some(disc) = s.last_disconnect_at else {
            return false;
        };
        if now.duration_since(disc) <= shutdown_grace {
            return false;
        }
        s.reaping_kill = true;
        true
    }

    /// Clear the `escalating` flag after successful signal delivery.
    async fn finish_escalation(&self) {
        let mut s = self.attach.lock().await;
        s.escalating = false;
    }

    /// Atomically mark the exec as doomed AND clear escalating. Used when
    /// signal delivery fails during escalation — ensures no gap between
    /// clearing escalating and setting reaping_kill where mark_connected
    /// could slip through.
    async fn escalation_failed_mark_doomed(&self) {
        let mut s = self.attach.lock().await;
        s.escalating = false;
        s.reaping_kill = true;
    }
}

// ============================================================================
// Phase 5.7 — Orphan reaper
// ============================================================================

const REAPER_TICK: std::time::Duration = std::time::Duration::from_secs(30);
const REAPER_SIGNAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Budget for one runtime call on the lifecycle sweep.
///
/// Stopping or removing a box is heavier than signalling an exec, so it gets a
/// wider budget than `REAPER_SIGNAL_TIMEOUT` — but it is still bounded: the
/// sweep shares its task with orphan-exec reaping, so an unbounded await on a
/// wedged microVM would stall that too. A call that times out leaves the
/// deadline unmet and the next tick retries it.
///
/// This bounds each call, not the pass: boxes are swept one at a time, as the
/// orphan reaper walks its own candidates, so a tick can still run long.
const LIFECYCLE_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const DEFAULT_RECONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(300);
const DEFAULT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
const DEFAULT_MAX_SESSION_LIFETIME: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
const COMPLETED_RETENTION_GRACE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Minimal duration parser: accepts `<n>s`, `<n>m`, `<n>h`, or a bare
/// integer interpreted as seconds. Mirrors Go's `time.ParseDuration` for
/// the cases we actually use. Returns `fallback` on any error or unset
/// env var, logging a warning so operators don't silently inherit the
/// default.
fn resolve_duration(var: &str, fallback: std::time::Duration) -> std::time::Duration {
    let raw = match std::env::var(var) {
        Ok(s) if !s.is_empty() => s,
        _ => return fallback,
    };
    let parsed = if let Some(rest) = raw.strip_suffix('h') {
        rest.parse::<u64>()
            .ok()
            .map(|n| std::time::Duration::from_secs(n * 3600))
    } else if let Some(rest) = raw.strip_suffix('m') {
        rest.parse::<u64>()
            .ok()
            .map(|n| std::time::Duration::from_secs(n * 60))
    } else if let Some(rest) = raw.strip_suffix('s') {
        rest.parse::<u64>().ok().map(std::time::Duration::from_secs)
    } else {
        raw.parse::<u64>().ok().map(std::time::Duration::from_secs)
    };
    match parsed {
        Some(d) => d,
        None => {
            tracing::warn!(env = var, value = %raw,
                "invalid duration env var (use Ns/Nm/Nh), using default");
            fallback
        }
    }
}

/// What the sweep should do with one box.
#[derive(Debug, PartialEq, Eq)]
enum LifecycleAction {
    Leave,
    Stop,
    Delete,
}

/// Decide one box's fate from its policy and observed clocks alone.
///
/// Split out from the sweep so the rule is testable without a VM, a runtime or
/// a clock. `0` disables either deadline, matching the wire contract.
fn decide_lifecycle(
    status: BoxStatus,
    auto_stop_secs: u32,
    auto_delete_secs: u32,
    idle: std::time::Duration,
    since_stop: std::time::Duration,
) -> LifecycleAction {
    // A running box is stopped once it has been idle for the whole window.
    if status == BoxStatus::Running
        && auto_stop_secs > 0
        && idle >= std::time::Duration::from_secs(u64::from(auto_stop_secs))
    {
        return LifecycleAction::Stop;
    }

    // A box that has come to rest is deleted once it has been at rest for the
    // whole window, measured from that transition rather than from last use: a
    // box that was busy right up to its stop must still age out on schedule.
    //
    // `Failed` counts as at rest — it will never run again on its own, so
    // excluding it would leak exactly the boxes nobody goes back to clean up.
    let at_rest = matches!(status, BoxStatus::Stopped | BoxStatus::Failed);
    if at_rest
        && auto_delete_secs > 0
        && since_stop >= std::time::Duration::from_secs(u64::from(auto_delete_secs))
    {
        return LifecycleAction::Delete;
    }

    LifecycleAction::Leave
}

/// How long a box has been idle, seeding the clock if this `serve` has not seen
/// it before.
///
/// The read guard must be released before the write lock is taken. Holding it
/// across the `.write().await` — which is what a `match` on the guard does,
/// since the scrutinee temporary outlives the arms — self-deadlocks on tokio's
/// `RwLock` and hangs the whole reaper loop, orphan-exec reaping included.
/// The clock is keyed by whatever spelling the caller used, so every stamp that
/// resolves to this box counts and the newest wins. `boxes` is the live set,
/// needed because resolving a prefix depends on what else exists. A box seen for
/// the first time is seeded under its canonical id.
async fn idle_or_seed(
    last_activity: &RwLock<HashMap<String, Instant>>,
    box_id: &str,
    boxes: &[(String, Option<String>)],
    now: Instant,
) -> std::time::Duration {
    let stamped = {
        let clocks = last_activity.read().await;
        clocks
            .iter()
            .filter(|(spelling, _)| box_named_by(spelling, boxes) == Some(box_id))
            .map(|(_, stamped)| *stamped)
            .max()
    };
    match stamped {
        Some(stamped) => now.saturating_duration_since(stamped),
        None => {
            last_activity.write().await.insert(box_id.to_string(), now);
            std::time::Duration::ZERO
        }
    }
}

/// The box a client's spelling resolves to, or `None` if it names none.
///
/// Mirrors `BoxManager::lookup_box` (litebox/manager.rs:102): an exact id, then
/// an exact name, then a prefix — and a prefix matching more than one box
/// resolves to nothing there, so it resolves to nothing here. The empty
/// spelling is the one divergence, rejected here rather than treated as a
/// prefix of everything; `activity_box_id` never yields one.
///
/// Resolving rather than enumerating, because prefixes cannot be listed: there
/// are as many as the id is long. Without this, `boxlite --url … exec 01HJK4`
/// would stamp a key the sweep never reads and its box would be AutoStopped
/// mid-use.
///
/// Rejecting the ambiguous prefix is what keeps that from becoming a lever.
/// `record_activity` stamps the raw path segment before any handler can reject
/// it, and `serve` may run with no API key — so if a one-character prefix
/// counted for every box it matched, `POST /v1/boxes/a/exec` would hold all of
/// them open, and 62 such requests a tick would pin the whole server.
fn box_named_by<'a>(spelling: &str, boxes: &'a [(String, Option<String>)]) -> Option<&'a str> {
    if spelling.is_empty() {
        return None;
    }
    if let Some((id, _)) = boxes.iter().find(|(id, _)| id == spelling) {
        return Some(id);
    }
    if let Some((id, _)) = boxes
        .iter()
        .find(|(_, name)| name.as_deref() == Some(spelling))
    {
        return Some(id);
    }
    let mut prefixed = boxes.iter().filter(|(id, _)| id.starts_with(spelling));
    match (prefixed.next(), prefixed.next()) {
        (Some((id, _)), None) => Some(id),
        _ => None,
    }
}

/// Boxes with work in flight, which AutoStop must not interrupt.
///
/// A tenant exec counts: `record_activity` stamps once per HTTP request, so a
/// long-running `POST /exec` never re-stamps and the box would look idle.
///
/// A `Main` session does not. It is the box's own init, it lives as long as the
/// box runs, and `run_reap_once` never evicts it — so counting it would pin
/// every started box and AutoStop could never fire at all. An idle box whose
/// init is still running is exactly what AutoStop exists to stop; an
/// *interactive* main session is kept alive by its data frames re-stamping the
/// clock instead.
fn busy_box_ids(
    executions: &HashMap<String, Arc<ActiveExecution>>,
) -> std::collections::HashSet<String> {
    executions
        .values()
        .filter(|execution| execution.kind() != SessionKind::Main && !execution.is_done())
        .map(|execution| execution.box_id.clone())
        .collect()
}

/// One lifecycle pass: stop idle boxes, delete boxes that have been at rest
/// long enough. Runs on the same tick as the orphan-exec reaper.
async fn run_lifecycle_once(state: &AppState, now: Instant) {
    let boxes = match tokio::time::timeout(LIFECYCLE_OP_TIMEOUT, state.runtime.list_info()).await {
        Ok(Ok(boxes)) => boxes,
        Ok(Err(error)) => {
            tracing::warn!(%error, "lifecycle sweep could not list boxes");
            return;
        }
        Err(_) => {
            tracing::warn!("lifecycle sweep timed out listing boxes");
            return;
        }
    };

    let busy = busy_box_ids(&*state.executions.read().await);

    // The live set every recorded spelling is resolved against. A client may
    // address a box as its canonical id, its name, or an unambiguous id prefix,
    // and the middleware stamps whichever was written — so `box_named_by`
    // resolves a spelling to the box it names, rather than trying to list a
    // box's spellings, which is impossible for prefixes. Resolving a prefix
    // needs the whole set, since ambiguity is a property of it.
    let live: Vec<(String, Option<String>)> = boxes
        .iter()
        .map(|info| (info.id.to_string(), info.name.clone()))
        .collect();
    state.retain_known_boxes(&live).await;

    let now_utc = chrono::Utc::now();
    for info in boxes {
        let box_id = info.id.to_string();
        // The policy lives here, not on `BoxInfo`: neither deadline is handed
        // to the runtime, so neither comes back from it.
        let policy = state.lifecycle_of(&box_id).await;
        if policy.is_empty() {
            continue;
        }
        if busy
            .iter()
            .any(|spelling| box_named_by(spelling, &live) == Some(box_id.as_str()))
        {
            continue;
        }

        let idle = idle_or_seed(&state.last_activity, &box_id, &live, now).await;
        // The delete deadline uses `last_updated`: for a box at rest that
        // transition *is* the stop, which is the anchor AutoDelete wants.
        let since_stop = (now_utc - info.last_updated)
            .to_std()
            .unwrap_or(std::time::Duration::ZERO);

        match decide_lifecycle(
            info.status,
            policy.auto_stop,
            policy.auto_delete,
            idle,
            since_stop,
        ) {
            LifecycleAction::Leave => {}
            LifecycleAction::Stop => {
                tracing::info!(
                    box_id = %box_id,
                    idle_secs = idle.as_secs(),
                    auto_stop = policy.auto_stop,
                    "AutoStop deadline reached, stopping box"
                );
                match tokio::time::timeout(LIFECYCLE_OP_TIMEOUT, state.runtime.get(&box_id)).await {
                    Ok(Ok(Some(bx))) => {
                        match tokio::time::timeout(LIFECYCLE_OP_TIMEOUT, bx.stop()).await {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                tracing::warn!(box_id = %box_id, %error, "AutoStop failed to stop box");
                            }
                            Err(_) => {
                                tracing::warn!(box_id = %box_id, "AutoStop timed out stopping box");
                            }
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(box_id = %box_id, %error, "AutoStop could not fetch box")
                    }
                    Err(_) => {
                        tracing::warn!(box_id = %box_id, "AutoStop timed out fetching box")
                    }
                }
            }
            LifecycleAction::Delete => {
                tracing::info!(
                    box_id = %box_id,
                    at_rest_secs = since_stop.as_secs(),
                    auto_delete = policy.auto_delete,
                    "AutoDelete deadline reached, removing box"
                );
                // Evict the cached handle first, as `remove_box` does: a removed
                // box is never fetched again, so nothing else would drop it.
                state.boxes.write().await.remove(&box_id);
                match tokio::time::timeout(
                    LIFECYCLE_OP_TIMEOUT,
                    state.runtime.remove(&box_id, false),
                )
                .await
                {
                    Ok(Ok(())) => state.forget_box(&box_id).await,
                    Ok(Err(error)) => {
                        tracing::warn!(box_id = %box_id, %error, "AutoDelete failed to remove box");
                    }
                    Err(_) => {
                        tracing::warn!(box_id = %box_id, "AutoDelete timed out removing box");
                    }
                }
            }
        }
    }
}

async fn reaper_loop(state: Arc<AppState>) {
    let reconnect_grace = resolve_duration("BOXLITE_RECONNECT_GRACE", DEFAULT_RECONNECT_GRACE);
    let shutdown_grace = resolve_duration("BOXLITE_SHUTDOWN_GRACE", DEFAULT_SHUTDOWN_GRACE);
    let max_lifetime =
        resolve_duration("BOXLITE_MAX_SESSION_LIFETIME", DEFAULT_MAX_SESSION_LIFETIME);

    let mut ticker = tokio::time::interval(REAPER_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = Instant::now();
        run_reap_once(&state, now, reconnect_grace, shutdown_grace, max_lifetime).await;
        run_lifecycle_once(&state, now).await;
    }
}

async fn run_reap_once(
    state: &AppState,
    now: Instant,
    reconnect_grace: std::time::Duration,
    shutdown_grace: std::time::Duration,
    max_lifetime: std::time::Duration,
) {
    let candidates: Vec<(String, Arc<ActiveExecution>)> = {
        let map = state.executions.read().await;
        map.iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect()
    };

    for (id, active) in candidates {
        // Done check first — a completed exec is always handled by the
        // retention path, even if it exceeds the lifetime cap. This
        // avoids starving done-eviction when try_kill_and_evict keeps
        // failing for an already-exited process.
        if active.is_done() {
            if !active.should_retain(now) {
                // Compare-and-remove, for the same reason as `try_kill_and_evict`:
                // a restarted box re-registers its main session under the same
                // container id, and this snapshot may already be stale.
                evict_if_same(state, &id, &active).await;
            }
            continue;
        }

        // A main session is never an orphan and never stale: it is the box's
        // init, so killing it powers the VM off and destroys the box. A client
        // walking away from `docker attach` does not stop the container, and
        // neither may this; nor may the lifetime cap, which exists to bound
        // *exec* sessions, not the workload they run beside.
        //
        // Every branch below signals or kills, so Main stops here. The
        // done-eviction above still applies to it — a main session whose init
        // has exited really is finished, and evicting a dead entry kills
        // nothing.
        if active.kind() == SessionKind::Main {
            continue;
        }

        if now.duration_since(active.created_at()) > max_lifetime {
            active.mark_reaping_kill().await;
            tracing::warn!(exec_id = %id, "session lifetime cap reached, killing");
            try_kill_and_evict(state, &id, &active).await;
            continue;
        }
        // Retry kill for entries already marked doomed by a prior tick
        // or a failed DELETE handler.
        if active.is_reaping_kill().await {
            try_kill_and_evict(state, &id, &active).await;
            continue;
        }
        if active.try_escalate_hup(now, reconnect_grace).await {
            let sig_result =
                tokio::time::timeout(REAPER_SIGNAL_TIMEOUT, active.execution().signal(1)).await;
            if matches!(sig_result, Ok(Ok(()))) {
                active.finish_escalation().await;
            } else {
                tracing::warn!(exec_id = %id, "SIGHUP delivery failed or timed out, killing");
                active.escalation_failed_mark_doomed().await;
                try_kill_and_evict(state, &id, &active).await;
            }
        } else if active.try_escalate_term(now, shutdown_grace).await {
            let sig_result =
                tokio::time::timeout(REAPER_SIGNAL_TIMEOUT, active.execution().signal(15)).await;
            if matches!(sig_result, Ok(Ok(()))) {
                active.finish_escalation().await;
            } else {
                tracing::warn!(exec_id = %id, "SIGTERM delivery failed or timed out, killing");
                active.escalation_failed_mark_doomed().await;
                try_kill_and_evict(state, &id, &active).await;
            }
        } else if active.try_escalate_kill(now, shutdown_grace).await {
            tracing::warn!(exec_id = %id, "orphan exec did not exit after SIGTERM, killing");
            try_kill_and_evict(state, &id, &active).await;
        }
    }
}

/// Kill and remove from the map. Only evicts on kill success; on failure
/// the entry stays with `reaping_kill=true` so the next reaper tick retries.
async fn try_kill_and_evict(state: &AppState, id: &str, active: &Arc<ActiveExecution>) {
    let result = tokio::time::timeout(REAPER_SIGNAL_TIMEOUT, active.execution().kill()).await;
    match result {
        Ok(Ok(())) => {
            evict_if_same(state, id, active).await;
        }
        Ok(Err(e)) => {
            tracing::warn!(exec_id = %id, err = %e, "kill failed, will retry next tick");
        }
        Err(_) => {
            tracing::warn!(exec_id = %id, "kill timed out, will retry next tick");
        }
    }
}

/// Remove `id` only if it still maps to the session we decided to reap.
///
/// The reaper works from a snapshot taken under an earlier read lock and awaits
/// in between (a kill can block for `REAPER_SIGNAL_TIMEOUT`), so by the time it
/// evicts, the key may have been rebound. That is not hypothetical for the main
/// session: its id *is* the container id, which is fixed at box creation, so a
/// box that restarts re-registers a brand-new session under the very same key.
/// A bare `remove(id)` would then delete the live session out from under its
/// client — and the client could not recover, because the guest refuses a second
/// `Attach` on a session that already has one.
async fn evict_if_same(state: &AppState, id: &str, doomed: &Arc<ActiveExecution>) {
    let mut map = state.executions.write().await;
    if let Some(current) = map.get(id)
        && Arc::ptr_eq(current, doomed)
    {
        map.remove(id);
    }
}

// ============================================================================
// Conversions
// ============================================================================

fn box_info_to_response(info: &BoxInfo) -> BoxResponse {
    BoxResponse {
        box_id: info.id.to_string(),
        name: info.name.clone(),
        status: info.status.as_str().to_string(),
        created_at: info.created_at.to_rfc3339(),
        updated_at: info.last_updated.to_rfc3339(),
        pid: info.pid,
        image: info.image.clone(),
        cpus: info.cpus,
        memory_mib: info.memory_mib,
        labels: info.labels.clone(),
        auto_stop: info.auto_stop,
        auto_delete: info.auto_delete,
        auto_resume: info.auto_resume,
        exit_code: info.exit_code,
    }
}

fn volume_info_to_response(info: &boxlite::runtime::types::VolumeInfo) -> types::VolumeResponse {
    types::VolumeResponse {
        id: info.id.clone(),
        name: info.name.clone(),
        created_at: info.created_at.to_rfc3339(),
        size_bytes: info.size_bytes,
    }
}

fn build_box_options(req: &CreateBoxRequest) -> Result<BoxOptions, boxlite::BoxliteError> {
    let rootfs = if let Some(ref path) = req.rootfs_path {
        RootfsSpec::RootfsPath(path.clone())
    } else {
        RootfsSpec::Image(req.image.clone().unwrap_or_else(|| "alpine:latest".into()))
    };

    let env: Vec<(String, String)> = req
        .env
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    let (network, inbound_network) = match &req.network {
        Some(network) => {
            if network.uses_legacy_fields()
                && (network.outbound.is_some() || network.inbound.is_some())
            {
                return Err(boxlite::BoxliteError::InvalidArgument(
                    "network must use either nested outbound/inbound fields or legacy flat fields, not both"
                        .into(),
                ));
            }

            let (mode, allow_net) = match &network.outbound {
                Some(outbound) => (
                    outbound.mode.parse::<NetworkMode>()?,
                    outbound.allow_net.clone(),
                ),
                None => (
                    network
                        .legacy
                        .mode
                        .as_deref()
                        .unwrap_or("enabled")
                        .parse::<NetworkMode>()?,
                    network.legacy.allow_net.clone().unwrap_or_default(),
                ),
            };
            let (inbound_mode, inbound_allow_net) = match &network.inbound {
                Some(inbound) => (
                    inbound.mode.parse::<NetworkMode>()?,
                    inbound.allow_net.clone(),
                ),
                None => (NetworkMode::Enabled, Vec::new()),
            };
            (
                NetworkSpec::try_from(OutboundNetworkConfig { mode, allow_net })?,
                NetworkSpec::try_from(InboundNetworkConfig {
                    mode: inbound_mode,
                    allow_net: inbound_allow_net,
                })?,
            )
        }
        None => (NetworkSpec::default(), NetworkSpec::default()),
    };

    // SecurityOptions is deliberately NOT client-configurable over
    // REST: sandbox security is the operator's policy. The server
    // always uses `AdvancedBoxOptions::default()` for new boxes, so
    // the default-flip (jailer + seccomp on for Linux/macOS) applies
    // uniformly. Operators who want a different policy run the
    // server with a different default; clients cannot relax it.

    // An empty name or value can never substitute anything. Reject at the
    // boundary so this server agrees with the Cloud API's IsNotEmpty and the
    // runner's per-element `dive` required validation on what a secret is.
    if let Some(secrets) = &req.secrets
        && secrets
            .iter()
            .any(|s| s.name.is_empty() || s.value.is_empty())
    {
        return Err(boxlite::BoxliteError::InvalidArgument(
            "secret name and value must be non-empty".into(),
        ));
    }

    // Map secrets onto the core `Secret` type and apply the placeholder
    // default. The local runtime does not synthesize `<BOXLITE_SECRET:{name}>`
    // for an empty placeholder (unlike the Go SDK), so defaulting here is what
    // keeps a placeholder-less secret from silently injecting an empty env var
    // and no MITM substitution — the same failure class POL-303 fixes on Cloud.
    let secrets: Vec<boxlite::runtime::options::Secret> = req
        .secrets
        .as_ref()
        .map(|ss| {
            ss.iter()
                .map(|s| boxlite::runtime::options::Secret {
                    name: s.name.clone(),
                    value: s.value.clone(),
                    hosts: s.hosts.clone(),
                    placeholder: s
                        .placeholder
                        .clone()
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| format!("<BOXLITE_SECRET:{}>", s.name)),
                })
                .collect()
        })
        .unwrap_or_default();

    // Every mount names a volume this server owns; the runtime resolves the
    // reference to a directory at create. A client cannot reach a host path
    // through this field — `CreateVolumeMount` has none.
    let volumes = req
        .volumes
        .as_ref()
        .map(|mounts| {
            mounts
                .iter()
                .map(|mount| {
                    let mut spec = boxlite::runtime::options::VolumeSpec::managed_volume(
                        mount.managed_volume.clone(),
                        mount.guest_path.clone(),
                    );
                    spec.read_only = mount.read_only;
                    spec
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(BoxOptions {
        rootfs,
        volumes,
        cpus: req.cpus,
        memory_mib: req.memory_mib,
        disk_size_gb: req.disk_size_gb,
        working_dir: req.working_dir.clone(),
        env,
        secrets,
        network,
        inbound_network,
        entrypoint: req.entrypoint.clone(),
        cmd: req.cmd.clone(),
        user: req.user.clone(),
        tty: req.tty.unwrap_or(false),
        advanced: {
            let mut advanced = boxlite::AdvancedBoxOptions::default();
            let capabilities = req.advanced.capabilities.as_ref().map(|capabilities| {
                boxlite::ContainerCapabilities {
                    add: capabilities.add.clone(),
                    drop: capabilities.drop.clone(),
                }
            });
            advanced.set_capabilities(capabilities)?;
            advanced
        },
        // Neither deadline is forwarded; `serve` holds both and sweeps them.
        //
        // `auto_stop` cannot be forwarded at all: the local runtime this server
        // drives refuses a non-zero value outright
        // (`reject_local_unsupported_options`, runtime/rt_impl.rs:1743), so a
        // create carrying one would 400 before anything could enforce it.
        //
        // `auto_delete` would be accepted but reinterpreted: `removes_on_stop()`
        // is `effective_auto_delete() > 0`, so the engine would delete the box
        // the moment it stopped rather than after the delay, and force it
        // non-detached. `create_box` records both in `AppState`.
        auto_stop: None,
        auto_delete: Some(0),
        auto_resume: req.auto_resume,
        // Boxes made over the wire outlive the request that made them.
        detach: req.detach.unwrap_or(true),
        ..Default::default()
    })
}

fn build_box_command(req: &ExecRequest) -> Result<BoxCommand, boxlite::BoxliteError> {
    let mut cmd = BoxCommand::new(&req.command).args(req.args.iter().map(String::as_str));

    if let Some(ref env_map) = req.env {
        for (k, v) in env_map {
            cmd = cmd.env(k, v);
        }
    }
    if let Some(ref wd) = req.working_dir {
        cmd = cmd.working_dir(wd);
    }
    if req.tty {
        cmd = cmd.tty(true);
    }
    if let Some(secs) = req.timeout_seconds {
        cmd = cmd.timeout_seconds(secs)?;
    }
    Ok(cmd)
}

// ============================================================================
// Error Helpers
// ============================================================================

/// Build a JSON error response with the canonical wire envelope.
///
/// `error_type` and `code` are caller-supplied because some sites
/// (auth middleware, handler timeout, schema-validation rejection) emit
/// errors that don't correspond to a `BoxliteError` variant. For
/// `BoxliteError` paths use [`error_from_boxlite`] instead — it dispatches
/// to the single source of truth in `BoxliteError::http()`.
fn error_response(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &str,
    code: &str,
) -> Response {
    let body = ErrorBody {
        error: ErrorDetail {
            message: message.into(),
            error_type: error_type.to_string(),
            code: code.to_string(),
            request_id: None,
        },
    };
    (status, Json(body)).into_response()
}

/// Map a `BoxliteError` to its canonical HTTP response. Delegates the
/// (status, type, code) decision to `BoxliteError::http()` so the mapping
/// is exhaustive at compile time — adding a new variant becomes a build
/// error in `errors.rs`, never a silent 500.
fn error_from_boxlite(err: &boxlite::BoxliteError) -> Response {
    let (code, etype, ecode) = err.http();
    let status = StatusCode::from_u16(code)
        .expect("BoxliteError::http() must return a valid HTTP status code");
    error_response(status, err.to_string(), etype, ecode)
}

/// Panic handler for [`CatchPanicLayer`]. Turns a handler panic into a
/// `500 InternalError internal` response with our wire envelope —
/// otherwise axum's default returns an empty `500 Internal Server Error`
/// with no body, breaking the client's status-table 500-vs-Network
/// distinction.
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = err
        .downcast_ref::<&'static str>()
        .map(|s| s.to_string())
        .or_else(|| err.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic in handler".to_string());
    tracing::error!(panic = %detail, "handler panicked");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("internal error: {}", detail),
        "InternalError",
        "internal",
    )
}

/// Pure auth decision (unit-tested). `true` = allow. `expected == None` ⇒
/// permissive (no key configured). `GET /v1/config` is always public
/// (pre-auth capability discovery). Otherwise the presented bearer must
/// match `expected` (constant-time).
fn auth_allows(expected: Option<&str>, path: &str, bearer: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    if path == "/v1/config" {
        return true;
    }
    match bearer {
        Some(tok) => constant_time_eq(tok.as_bytes(), expected.as_bytes()),
        None => false,
    }
}

/// Whether a request path is user activity on a box, and if so which box.
///
/// Box-scoped paths count by default. Defaulting to "counts" means a newly
/// added box operation cannot silently become invisible to the idle clock and
/// get its box stopped mid-flight.
///
/// The exclusions are the paths a poller hits on a timer, which must never be
/// able to hold a box open forever:
///
/// - `/metrics`, scraped on a schedule;
/// - a bare `GET`/`HEAD` on the box, which reads metadata rather than using the
///   box — a client watching `status` once a second would otherwise reset the
///   idle window before every sweep and AutoStop would never fire;
/// - `DELETE`, which is removing the box, not using it.
///
/// `/stop` is excluded because it cannot matter: AutoStop ignores a box that is
/// already stopped, and AutoDelete measures from `last_updated`, not this clock.
fn activity_box_id<'a>(method: &Method, path: &'a str) -> Option<&'a str> {
    let rest = path.strip_prefix("/v1/boxes/")?;
    let (box_id, tail) = match rest.split_once('/') {
        Some((box_id, tail)) => (box_id, tail),
        None => (rest, ""),
    };
    if box_id.is_empty() || matches!(tail, "metrics" | "stop") {
        return None;
    }
    // A read of the box resource itself is metadata, not use.
    if tail.is_empty() && matches!(*method, Method::GET | Method::HEAD | Method::DELETE) {
        return None;
    }
    Some(box_id)
}

/// Stamp the idle clock for the box a request names, before it runs.
async fn record_activity(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(segment) = activity_box_id(req.method(), req.uri().path()) {
        // Decoded before it is stamped: the request path is raw, while the
        // sweep resolves it through `box_named_by`, against names that arrive
        // decoded. Any
        // escaped spelling would file the box's activity under a key the sweep
        // never queries, and the box would be AutoStopped while in continuous
        // use. A client may escape an unreserved character in a perfectly legal
        // name (`my%2Dbox` for `my-box`), so this does not rest on the contract's
        // `name` pattern being unenforced here — which it is.
        let box_id = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
        state.record_box_activity(&box_id).await;
    }
    next.run(req).await
}

/// Auth middleware: thin axum adapter over [`auth_allows`]. 401 in the
/// standard error shape when denied.
async fn require_api_key(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        });
    if auth_allows(state.api_key.as_deref(), req.uri().path(), bearer) {
        next.run(req).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            "invalid or missing API key",
            "AuthError",
            "unauthenticated",
        )
    }
}

/// Length-checked constant-time byte compare — avoids a timing oracle on the
/// configured token without pulling in a crate.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ============================================================================
// Box Handle Cache Helper
// ============================================================================

/// Whether an operation that would wake this box is allowed to proceed.
///
/// The flag governs *resuming* — bringing back a box that ran and was stopped,
/// which is what AutoStop leaves behind. Only `Stopped` is that state.
///
/// Every other state is left alone deliberately. `Running`/`Paused` are already
/// up, so no wake is involved. A `Configured` box has never run, and its first
/// boot is not a resume: gating it would break `run --url`, which attaches
/// before it starts the box, so `run --no-auto-resume` would refuse itself.
/// `Failed` and `Stopping` are not states a caller asked to come back from.
fn autoresume_allows(status: BoxStatus, auto_resume: bool) -> bool {
    status != BoxStatus::Stopped || auto_resume
}

/// Resolve a box for an operation that drives the guest, refusing to wake it
/// implicitly when the caller disabled AutoResume.
///
/// Only the operations that would actually boot the box use this — exec, files,
/// attach. `start`/`stop`, snapshots, clone and export resolve through
/// `get_or_fetch_box`: they either are the explicit lifecycle call or work off
/// disk, and gating them would answer a metadata read with advice to start a box
/// the caller never asked to run. Metrics boots too but never resumes, so it has
/// its own resolver: see `get_box_for_metrics`.
#[allow(clippy::result_large_err)]
async fn get_or_resume_box(state: &AppState, box_id: &str) -> Result<Arc<LiteBox>, Response> {
    // `info()` reads persisted state and does not boot anything, so asking it
    // first cannot itself perform the wake we may be about to refuse.
    let info = match state.runtime.get_info(box_id).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("box not found: {box_id}"),
                "NotFoundError",
                "not_found",
            ));
        }
        Err(e) => return Err(error_from_boxlite(&e)),
    };

    if !autoresume_allows(info.status, info.auto_resume) {
        return Err(error_response(
            StatusCode::CONFLICT,
            format!(
                "box {box_id} is {:?} and has AutoResume disabled; start it explicitly first",
                info.status
            ),
            "ConflictError",
            "conflict",
        ));
    }

    get_or_fetch_box(state, box_id).await
}

/// Resolve a box for a metrics scrape, refusing to wake one the sweep would put
/// straight back to sleep.
///
/// `serve` must not wake a box for an operation it does not count as use, and
/// metrics is the only such operation: `LiteBox::metrics()` boots a box that is
/// not running, while `activity_box_id` deliberately does not stamp the idle
/// clock for a scrape. Together, on a box with an AutoStop deadline, they are a
/// loop — the scrape wakes the box, the next tick finds it idle and stops it,
/// the scrape after that wakes it again, each wake re-running the image's init.
/// That is also what the guide has always promised: monitoring must not be able
/// to start a box, or to hold one open.
///
/// Only a box the sweep would stop is refused. One with no AutoStop deadline
/// keeps the historical boot-on-scrape: nothing would put it back to sleep, so
/// there is no loop to break and no reason to change what a scrape did before.
#[allow(clippy::result_large_err)]
async fn get_box_for_metrics(state: &AppState, box_id: &str) -> Result<Arc<LiteBox>, Response> {
    // `get_info` reads persisted state and boots nothing, so asking it first
    // cannot itself perform the wake we may be about to refuse.
    let info = match state.runtime.get_info(box_id).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("box not found: {box_id}"),
                "NotFoundError",
                "not_found",
            ));
        }
        Err(e) => return Err(error_from_boxlite(&e)),
    };

    // The deadline is looked up by the box's own id, never by the spelling the
    // request used. The path segment may be a user-defined name (the wire
    // contract says "the identifier, or a user-defined name"), while `create_box`
    // only ever files the policy under the canonical id — so keying off the
    // segment would read no deadline for `/v1/boxes/{name}/metrics` and wake the
    // very box this exists to leave asleep. The sweep resolves the same aliasing
    // from the other side, in `box_named_by`.
    let policy = state.lifecycle_of(info.id.as_ref()).await;
    if metrics_would_thrash(info.status, policy.auto_stop) {
        let status = info.status;
        return Err(error_response(
            StatusCode::CONFLICT,
            format!(
                "box {box_id} is {status:?} and AutoStop is enabled; a metrics scrape does not \
                 start a box, start it explicitly first"
            ),
            "ConflictError",
            "conflict",
        ));
    }

    get_or_fetch_box(state, box_id).await
}

/// Whether serving metrics would wake a box the sweep would then stop again.
fn metrics_would_thrash(status: BoxStatus, auto_stop_secs: u32) -> bool {
    status != BoxStatus::Running && auto_stop_secs > 0
}

#[allow(clippy::result_large_err)]
async fn get_or_fetch_box(state: &AppState, box_id: &str) -> Result<Arc<LiteBox>, Response> {
    // Check cache first.
    //
    // A cached handle is only good while its box is up. A box can now stop
    // *itself* — its main command exits and the guest powers the VM off — and
    // such a handle is spent: it holds the dead VM and can never boot another.
    // The runtime's own cache is invalidated by the exit watcher, but this one is
    // ours, and nothing was clearing it. Serving from it would answer every later
    // `/exec`, `/files` and `/attach` on that box with a corpse, forever.
    //
    // So a non-Running box gets a fresh handle, which *can* boot it. That is the
    // auto-restart the cloud depends on: its reaper stops idle boxes and the next
    // SDK call is expected to bring them back.
    let cached = state.boxes.read().await.get(box_id).cloned();
    if let Some(cached) = cached {
        match cached.info().await {
            Ok(info) if info.status.is_active() => return Ok(Arc::clone(&cached)),
            Ok(_) => {}
            Err(e) => return Err(error_from_boxlite(&e)),
        }
        let mut boxes = state.boxes.write().await;
        if boxes
            .get(box_id)
            .is_some_and(|current| Arc::ptr_eq(current, &cached))
        {
            boxes.remove(box_id);
        }
    }

    // Fetch from runtime
    match state.runtime.get(box_id).await {
        Ok(Some(b)) => {
            let id = b
                .info()
                .await
                .map_err(|e| error_from_boxlite(&e))?
                .id
                .to_string();
            let arc = Arc::new(b);
            state.boxes.write().await.insert(id, Arc::clone(&arc));
            Ok(arc)
        }
        Ok(None) => Err(error_response(
            StatusCode::NOT_FOUND,
            format!("box not found: {box_id}"),
            "NotFoundError",
            "not_found",
        )),
        Err(e) => Err(error_from_boxlite(&e)),
    }
}

// ============================================================================
// Main Session (container init)
// ============================================================================

/// Find a box's already-open main session in the registry.
///
/// Linear, because the registry is keyed by execution id and the main
/// session's id (the container id) is exactly what the caller doesn't
/// know — see [`SessionKind`]. The registry holds one entry per live
/// session on this server, so the scan is bounded by that, and it only
/// runs on attach.
fn find_main_session(
    executions: &HashMap<String, Arc<ActiveExecution>>,
    box_id: &str,
) -> Option<Arc<ActiveExecution>> {
    executions
        .values()
        // `is_done()` matters here in a way it never did for execs. A main
        // session's id is the container id, which is fixed at box creation and
        // is therefore the *same across reboots* — so a finished session from
        // the previous run would still match this box, and an attach after a
        // restart would be handed the old VM's dead stream, its stale backlog
        // and its stale exit code. An exec cannot collide that way: it gets a
        // fresh id every time.
        .find(|active| {
            active.box_id() == box_id && active.kind() == SessionKind::Main && !active.is_done()
        })
        .cloned()
}

/// Return the box's main session, calling `open` to create it only if the
/// box does not have one yet.
///
/// `open` runs at most once per box while the session is registered: the
/// guest binds init's stdout/stderr to the *first* `Attach` RPC and
/// answers later ones with `already_exists` (guest `ExecState::attach`),
/// so a second `LiteBox::attach()` would hand back a permanently silent
/// `Execution` — and, whichever one won the registry, leave a client
/// attached to a dead stream. The caller therefore holds the registry
/// write lock across this whole function, which is what makes the
/// check-then-open atomic against a concurrent first attach.
async fn register_main_session<F, Fut>(
    executions: &mut HashMap<String, Arc<ActiveExecution>>,
    box_id: &str,
    open: F,
) -> Result<Arc<ActiveExecution>, boxlite::BoxliteError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Execution, boxlite::BoxliteError>>,
{
    if let Some(active) = find_main_session(executions, box_id) {
        return Ok(active);
    }

    let mut execution = open().await?;
    let stdin = execution.stdin();
    let exec_id = execution.id().clone();
    let active = ActiveExecution::new(box_id.to_string(), SessionKind::Main, execution, stdin);
    executions.insert(exec_id, Arc::clone(&active));
    Ok(active)
}

/// Get the box's main session for `GET /boxes/{box_id}/attach`, opening it
/// on the first attach.
///
/// A cold box is now booted *inside* the registry write lock, which the earlier
/// design deliberately avoided. It has to be: the client must be attached before
/// the main command runs (create → attach → start), so the attach is no longer
/// separable from the boot, and the check-then-open has to stay atomic — the
/// guest refuses a second Attach on one session, so a racing client would end up
/// holding a permanently silent `Execution`. The cost is stated plainly at the
/// call site and it is not small: a cold boot pulls the image, which is
/// unbounded, and everything else queues behind it.
///
/// It deliberately does NOT call `start()` first. `start()` on a *Stopped* box
/// runs the restart pipeline, and the box's init is the user's main command —
/// so attaching to a finished job would silently run the job again. `attach()`
/// already does the right thing for every status by itself: it boots a
/// `Configured` box (which has never run) and refuses a `Stopped` one.
#[allow(clippy::result_large_err)]
async fn get_or_attach_main_session(
    state: &AppState,
    box_id: &str,
) -> Result<Arc<ActiveExecution>, Response> {
    if let Some(active) = find_main_session(&*state.executions.read().await, box_id) {
        return Ok(active);
    }

    let litebox = get_or_resume_box(state, box_id).await?;

    // Attaching boots the box (creating its container) and subscribes to the main
    // command's session, but does *not* run init — `POST /start` does. So a client
    // mid `run --url` is registered here, on the stream, before it starts the box:
    // docker's create → attach → start, split across the two calls it makes.
    //
    // This runs under the registry write lock, and booting a cold box pulls its
    // image, which is unbounded, so every other `/exec`, `/attach` and the reaper
    // wait behind it. The lock buys the atomic check-then-open that stops two
    // clients opening two guest streams for one session — the guest refuses a
    // second Attach, so the loser would get a permanently silent Execution. A
    // per-box open lock would scope that to the same box; worth doing, not here.
    let mut executions = state.executions.write().await;
    register_main_session(&mut executions, box_id, || async {
        litebox.attach(boxlite::AttachOptions::main()).await
    })
    .await
    .map_err(|e| error_from_boxlite(&e))
}

// ============================================================================
// Router
// ============================================================================

fn build_router(state: Arc<AppState>) -> Router {
    use handlers::{advanced, boxes, config, executions, files, me, metrics, snapshots, volumes};

    Router::new()
        // Identity (no tenant prefix)
        .route("/v1/me", get(me::get_me))
        .route("/v1/config", get(config::get_config))
        // Runtime metrics
        .route("/v1/metrics", get(metrics::runtime_metrics))
        // Named volumes
        .route(
            "/v1/volumes",
            post(volumes::create_volume).get(volumes::list_volumes),
        )
        .route(
            "/v1/volumes/{id}",
            get(volumes::get_volume).delete(volumes::remove_volume),
        )
        // Box CRUD (import first — static path before param path)
        .route("/v1/boxes/import", post(advanced::import_box))
        .route(
            "/v1/boxes",
            post(boxes::create_box).get(boxes::list_boxes),
        )
        .route(
            "/v1/boxes/{box_id}",
            get(boxes::get_box)
                .delete(boxes::remove_box)
                .head(boxes::head_box),
        )
        // Box lifecycle
        .route(
            "/v1/boxes/{box_id}/start",
            post(boxes::start_box),
        )
        .route(
            "/v1/boxes/{box_id}/stop",
            post(boxes::stop_box),
        )
        // Box metrics
        .route(
            "/v1/boxes/{box_id}/metrics",
            get(metrics::box_metrics),
        )
        // Main command session (container init) — docker's
        // `POST /containers/{id}/attach`, distinct from exec-attach below.
        .route(
            "/v1/boxes/{box_id}/attach",
            get(executions::attach_box),
        )
        // Execution
        .route(
            "/v1/boxes/{box_id}/exec",
            post(executions::start_execution),
        )
        .route(
            "/v1/boxes/{box_id}/executions/{exec_id}",
            get(executions::get_execution).delete(executions::kill_execution),
        )
        .route(
            "/v1/boxes/{box_id}/executions/{exec_id}/attach",
            get(executions::attach_execution),
        )
        .route(
            "/v1/boxes/{box_id}/executions/{exec_id}/signal",
            post(executions::send_signal),
        )
        .route(
            "/v1/boxes/{box_id}/executions/{exec_id}/resize",
            post(executions::resize_tty),
        )
        // Files
        .route(
            "/v1/boxes/{box_id}/files",
            put(files::upload_files).get(files::download_files),
        )
        // Snapshots
        .route(
            "/v1/boxes/{box_id}/snapshots",
            post(snapshots::create_snapshot).get(snapshots::list_snapshots),
        )
        .route(
            "/v1/boxes/{box_id}/snapshots/{name}",
            get(snapshots::get_snapshot).delete(snapshots::delete_snapshot),
        )
        .route(
            "/v1/boxes/{box_id}/snapshots/{name}/restore",
            post(snapshots::restore_snapshot),
        )
        // Clone & export
        .route(
            "/v1/boxes/{box_id}/clone",
            post(advanced::clone_box),
        )
        .route(
            "/v1/boxes/{box_id}/export",
            post(advanced::export_box),
        )
        // Applied before `require_api_key`, which therefore wraps it: an
        // unauthenticated request must not be able to hold someone else's box
        // open by resetting its idle clock.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_activity,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        // Middleware stack (outermost first, applied in reverse):
        // 1. SetRequestIdLayer — read X-Request-Id from request, or mint
        //    a UUID. Stored in request extensions for downstream handlers
        //    and tracing spans.
        // 2. PropagateRequestIdLayer — copy the request-id onto the
        //    response headers so clients can correlate to server logs.
        // 3. CatchPanicLayer — handler panic ⇒ 500 with our envelope.
        //    Without this, axum returns an empty 500 which the client
        //    mis-classifies as a proxy/Network error.
        //
        // Skipped (intentionally): TimeoutLayer. boxlite handlers have
        // operation-specific timeouts (signal/kill use 10s, image pulls
        // can legitimately take minutes). A global request timeout would
        // break long-running ops.
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(state)
}

// ============================================================================
// Entry Point
// ============================================================================

pub async fn execute(args: ServeArgs, global: &GlobalFlags) -> anyhow::Result<()> {
    // A server owns an embedded runtime. Client connection settings must not
    // turn it into a proxy merely because a credential profile or REST URL is
    // present in the environment.
    let options = global.resolve_runtime_options()?;
    let runtime = global.create_runtime_with_options(options)?;

    let state = Arc::new(AppState {
        runtime,
        boxes: RwLock::new(HashMap::new()),
        executions: RwLock::new(HashMap::new()),
        api_key: args.api_key.clone(),
        lifecycle: RwLock::new(HashMap::new()),
        last_activity: RwLock::new(HashMap::new()),
    });

    // Phase 5.7: spawn the orphan reaper. Same escalation policy as the
    // Go runner — 5min SIGHUP, +30s SIGTERM, +30s SIGKILL, 24h cap.
    tokio::spawn(reaper_loop(Arc::clone(&state)));

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    let addr = listener.local_addr()?;

    tracing::info!("boxlite serve listening on {}", addr);
    eprintln!("BoxLite REST API server listening on http://{addr}");

    // Graceful shutdown on ctrl-c
    let shutdown_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down...");
            eprintln!("\nShutting down...");
            let _ = shutdown_state.runtime.shutdown(Some(10)).await;
        })
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxlite::runtime::options::NetworkSpec;
    use std::time::Duration;

    // --- API-key auth decision (pure; no runtime/network needed) ---

    #[test]
    fn auth_allows_permissive_when_no_key() {
        assert!(auth_allows(None, "/v1/boxes", None));
        assert!(auth_allows(None, "/v1/me", Some("anything")));
    }

    #[test]
    fn auth_allows_config_public_even_with_key() {
        assert!(auth_allows(Some("k"), "/v1/config", None));
    }

    #[test]
    fn auth_allows_requires_exact_bearer_when_key_set() {
        assert!(auth_allows(Some("k"), "/v1/me", Some("k")));
        assert!(!auth_allows(Some("k"), "/v1/me", Some("wrong")));
        assert!(!auth_allows(Some("k"), "/v1/me", None));
        assert!(!auth_allows(Some("k"), "/v1/boxes", Some("")));
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn build_box_command_rejects_invalid_timeout_seconds() {
        for seconds in [-1.0, f64::NAN, f64::INFINITY] {
            let req = ExecRequest {
                command: "true".to_string(),
                args: Vec::new(),
                stdin: None,
                env: None,
                timeout_seconds: Some(seconds),
                working_dir: None,
                tty: false,
            };

            let err = build_box_command(&req).expect_err("invalid timeout should fail");
            assert!(
                matches!(err, boxlite::BoxliteError::InvalidArgument(ref msg) if msg.contains("timeout_seconds")),
                "unexpected error for {seconds:?}: {err}"
            );
        }
    }

    // ============================================================
    // REST `security` wire contract: server-owned only.
    //
    // The REST surface deliberately exposes no knob for clients to
    // pick a security preset or override `SecurityOptions`. Combined
    // with `#[serde(deny_unknown_fields)]` on `CreateBoxRequest`,
    // any client attempt to send `security` / `security_settings`
    // is rejected at deserialize time (i.e. 400 from the API)
    // rather than silently relaxing the server's policy.
    // ============================================================

    /// `-t` has to survive the wire, or a REST `run -it` silently gets pipes.
    ///
    /// The terminal belongs to the container's init, so it is decided at
    /// *create* and nothing downstream can add it. The client only sends the
    /// field when asked (the server rejects unknown fields), so both shapes
    /// must work: present-and-true, and absent.
    #[test]
    fn build_box_options_carries_tty_from_the_wire() {
        let with_tty: super::types::CreateBoxRequest =
            serde_json::from_str(r#"{"image": "alpine:latest", "tty": true}"#)
                .expect("body with tty must deserialize");
        assert!(
            build_box_options(&with_tty).expect("build").tty,
            "a REST client asking for a terminal must get one"
        );

        let without: super::types::CreateBoxRequest =
            serde_json::from_str(r#"{"image": "alpine:latest"}"#).expect("body must deserialize");
        assert!(
            !build_box_options(&without).expect("build").tty,
            "no tty asked for, none granted"
        );
    }

    #[test]
    fn build_box_options_carries_secrets_from_the_wire() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{"image":"alpine:latest","secrets":[{"name":"openai","value":"sk-test","hosts":["api.openai.com"]}]}"#,
        )
        .expect("body with secrets must deserialize");

        let opts = build_box_options(&req).expect("build with secrets");
        assert_eq!(opts.secrets.len(), 1, "one secret in, one secret out");
        let secret = &opts.secrets[0];
        assert_eq!(secret.name, "openai");
        assert_eq!(secret.value, "sk-test");
        assert_eq!(secret.hosts, vec!["api.openai.com"]);
        // Placeholder omitted on the wire: serve applies the same default the
        // Go SDK does, so a placeholder-less secret still substitutes.
        assert_eq!(secret.placeholder, "<BOXLITE_SECRET:openai>");
    }

    #[test]
    fn build_box_options_preserves_explicit_secret_placeholder() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{"image":"alpine:latest","secrets":[{"name":"httpbin","value":"v","placeholder":"<MY_TOKEN>"}]}"#,
        )
        .expect("body with explicit placeholder must deserialize");

        let opts = build_box_options(&req).expect("build");
        assert_eq!(
            opts.secrets[0].placeholder, "<MY_TOKEN>",
            "caller placeholder wins"
        );
    }

    #[test]
    fn build_box_options_defaults_an_empty_secret_placeholder() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{"image":"alpine:latest","secrets":[{"name":"openai","value":"v","placeholder":""}]}"#,
        )
        .expect("body with empty placeholder must deserialize");

        let opts = build_box_options(&req).expect("build");
        assert_eq!(
            opts.secrets[0].placeholder, "<BOXLITE_SECRET:openai>",
            "an empty placeholder is as absent as an omitted one"
        );
    }

    #[test]
    fn build_box_options_rejects_an_empty_secret_name_or_value() {
        for secrets in [
            r#"[{"name":"","value":"v"}]"#,
            r#"[{"name":"n","value":""}]"#,
        ] {
            let req: super::types::CreateBoxRequest = serde_json::from_str(&format!(
                r#"{{"image":"alpine:latest","secrets":{secrets}}}"#
            ))
            .expect("body with secrets must deserialize");

            let err = build_box_options(&req).expect_err("empty secret fields must be rejected");
            assert!(
                matches!(err, boxlite::BoxliteError::InvalidArgument(ref msg) if msg.contains("non-empty")),
                "unexpected error: {err}"
            );
        }
    }

    /// A mount names a volume; the runtime turns that reference into a
    /// directory. The wire carries no host path, and `deny_unknown_fields`
    /// is what keeps a client from adding one.
    #[test]
    fn build_box_options_carries_volume_mounts_from_the_wire() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{"image":"alpine:latest","volumes":[{"managed_volume":"my-data","guest_path":"/data","read_only":true}]}"#,
        )
        .expect("a mount request must deserialize");

        let options = build_box_options(&req).expect("volume mounts are supported");
        assert_eq!(1, options.volumes.len());
        assert_eq!(
            Some("my-data"),
            options.volumes[0].managed_volume.as_deref()
        );
        assert_eq!("/data", options.volumes[0].guest_path);
        assert!(options.volumes[0].read_only);
        assert!(
            options.volumes[0].host_path.is_empty(),
            "a client must not be able to name a host path"
        );

        let smuggled: Result<super::types::CreateBoxRequest, _> = serde_json::from_str(
            r#"{"image":"alpine:latest","volumes":[{"managed_volume":"v1","guest_path":"/data","host_path":"/etc"}]}"#,
        );
        assert!(smuggled.is_err(), "host_path must not deserialize");
    }

    #[test]
    fn build_box_options_carries_container_capabilities_from_the_wire() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{"image":"alpine:latest","advanced":{"capabilities":{"add":["SYS_ADMIN"],"drop":["CAP_NET_RAW"]}}}"#,
        )
        .expect("capability request must deserialize");

        let opts = build_box_options(&req).expect("build capability options");
        let capabilities = opts.advanced.capabilities().expect("capabilities set");
        assert_eq!(capabilities.add, vec!["SYS_ADMIN"]);
        assert_eq!(capabilities.drop, vec!["CAP_NET_RAW"]);
    }

    /// A request that never mentions `advanced`/`capabilities` at all must
    /// resolve to `None` (unspecified), not an explicit empty policy — that
    /// distinction is what a privileged request needs, and what an older
    /// archive importer needs (`archive_version_for_options` keys off it).
    #[test]
    fn build_box_options_leaves_capabilities_unspecified_when_the_wire_omits_them() {
        let req: super::types::CreateBoxRequest =
            serde_json::from_str(r#"{"image":"alpine:latest"}"#)
                .expect("ordinary request must deserialize");

        let opts = build_box_options(&req).expect("build ordinary options");
        assert!(
            opts.advanced.capabilities().is_none(),
            "omitting capabilities on the wire must not become an explicit empty policy"
        );
    }

    #[test]
    fn build_box_options_carries_lifecycle_policy_and_uses_compatible_detach_default() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{"auto_stop": 900, "auto_delete": 3600, "auto_resume": false}"#,
        )
        .expect("lifecycle body must deserialize");
        let opts = build_box_options(&req).expect("build lifecycle options");
        assert_eq!(
            opts.auto_stop, None,
            "the local runtime refuses a non-zero auto_stop outright, so \
             forwarding it would 400 the create"
        );
        assert_eq!(opts.auto_resume, Some(false));
        assert_eq!(
            opts.auto_delete,
            Some(0),
            "the deadline must not reach the runtime, which would read any \
             non-zero value as remove-on-stop and delete the box at its stop"
        );
        assert!(
            opts.detach,
            "withholding the deadline is what keeps a deadlined box detachable"
        );

        let persistent: super::types::CreateBoxRequest =
            serde_json::from_str(r#"{"auto_delete": 0}"#).expect("body must deserialize");
        assert!(
            build_box_options(&persistent).expect("build").detach,
            "persistent boxes keep the serve API's historical detached default"
        );
    }

    fn lifecycle_state() -> AppState {
        let runtime = BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .expect("rest runtime");
        AppState {
            runtime,
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        }
    }

    /// A `BoxInfo` shaped exactly as the runtime hands one back for a box
    /// created through `serve`: `build_box_options` withheld both deadlines, so
    /// the runtime recorded zeroes for them.
    fn info_as_the_runtime_reports_it(status: &str) -> BoxInfo {
        serde_json::from_value(serde_json::json!({
            "id": "01HJK4TNRPQSXYZ8WM6NCVT9R5",
            "name": "swept",
            "status": status,
            "created_at": "2026-01-01T00:00:00Z",
            "last_updated": "2026-01-01T00:00:00Z",
            "pid": null,
            "image": "alpine:latest",
            "cpus": 1,
            "memory_mib": 512,
            "labels": {},
            "auto_stop": 0,
            "auto_delete": 0,
            "auto_resume": true,
            "health_status": { "state": "None", "failures": 0, "last_check": null },
            "exit_code": null,
        }))
        .expect("box info")
    }

    /// Every box response must report the deadlines `serve` holds, not the
    /// zeroes it handed the runtime.
    ///
    /// `create_box` patching its own response is not enough: a client reading
    /// the box back — `GET`, the list, or the response to its own `stop` —
    /// would be told no deadline exists on a box the sweep is enforcing one
    /// for, and an SDK would copy that straight into its own `BoxInfo`.
    #[tokio::test]
    async fn a_box_response_reports_the_deadlines_serve_holds() {
        let state = lifecycle_state();
        let info = info_as_the_runtime_reports_it("stopped");
        assert_eq!(
            (info.auto_stop, info.auto_delete),
            (0, 0),
            "the runtime has no deadline to report; the whole point is that serve holds it"
        );

        state
            .set_lifecycle(
                info.id.as_ref(),
                LifecyclePolicy {
                    auto_stop: 900,
                    auto_delete: 604_800,
                },
            )
            .await;

        let resp = state.box_response(&info).await;
        assert_eq!(resp.auto_stop, 900, "AutoStop must survive the round trip");
        assert_eq!(
            resp.auto_delete, 604_800,
            "AutoDelete must survive the round trip"
        );
        assert!(
            resp.auto_resume,
            "auto_resume is forwarded to the runtime, so it is read off the info"
        );
    }

    /// A box `serve` holds no policy for reports no deadline — a clone or an
    /// import must not inherit the source box's.
    #[tokio::test]
    async fn a_box_with_no_stored_policy_reports_no_deadline() {
        let state = lifecycle_state();
        let resp = state
            .box_response(&info_as_the_runtime_reports_it("running"))
            .await;
        assert_eq!((resp.auto_stop, resp.auto_delete), (0, 0));
    }

    /// A metrics scrape must not wake a box the sweep would stop again.
    ///
    /// `LiteBox::metrics()` boots a box that is not running, and a scrape never
    /// stamps the idle clock — so on a box with an AutoStop deadline the two
    /// are a loop: scrape wakes, next tick stops, next scrape wakes. A box with
    /// no AutoStop deadline has nothing to put it back to sleep, so it keeps
    /// the historical boot-on-scrape.
    #[test]
    fn metrics_refuses_to_wake_only_a_box_the_sweep_would_stop() {
        for status in [
            BoxStatus::Unknown,
            BoxStatus::Configured,
            BoxStatus::Stopping,
            BoxStatus::Stopped,
            BoxStatus::Paused,
            BoxStatus::Failed,
        ] {
            assert!(
                metrics_would_thrash(status, 900),
                "{status:?} is not running, so serving metrics would boot it into the sweep"
            );
            assert!(
                !metrics_would_thrash(status, 0),
                "{status:?} has no AutoStop deadline, so nothing would stop it again"
            );
        }

        assert!(
            !metrics_would_thrash(BoxStatus::Running, 900),
            "a running box is not booted by a scrape, so there is no wake to refuse"
        );
        assert!(!metrics_would_thrash(BoxStatus::Running, 0));
    }

    /// The engine used to reject an invalid pair for us; now that neither
    /// deadline is forwarded, nothing behind this handler would. The runtime
    /// here points at a dead port on purpose: a 400 must come back without the
    /// request ever reaching `runtime.create`.
    #[tokio::test]
    async fn an_invalid_lifecycle_pair_is_refused_before_the_box_is_created() {
        let req: types::CreateBoxRequest =
            serde_json::from_str(r#"{"auto_stop": 3600, "auto_delete": 60}"#)
                .expect("body must deserialize");

        let response = handlers::boxes::create_box(
            axum::extract::State(Arc::new(lifecycle_state())),
            axum::Json(req),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "auto_delete <= auto_stop is forbidden by the contract"
        );
    }

    #[tokio::test]
    async fn only_a_tenant_exec_marks_a_box_busy() {
        // Main is the box's own init: it lives as long as the box and
        // `run_reap_once` never evicts it, so counting it would pin every
        // started box and AutoStop could never fire. A tenant exec must count,
        // or a long `POST /exec` looks idle and is killed mid-run.
        let (main_exec, _main_ch) = stub_execution("cid-main");
        let (tenant, _tenant_ch) = stub_execution("cid-exec");

        let mut executions: HashMap<String, Arc<ActiveExecution>> = HashMap::new();
        executions.insert(
            "main".to_string(),
            ActiveExecution::new("box-main".to_string(), SessionKind::Main, main_exec, None),
        );
        executions.insert(
            "exec".to_string(),
            ActiveExecution::new("box-exec".to_string(), SessionKind::Exec, tenant, None),
        );

        let busy = busy_box_ids(&executions);

        assert!(busy.contains("box-exec"), "a live tenant exec is work");
        assert!(
            !busy.contains("box-main"),
            "counting Main would make AutoStop unreachable for every started box"
        );

        // `ActiveExecution::new` spawns a pump and a wait task per session, and
        // they stay parked for as long as the stub's channels are open — which
        // has to be past `busy_box_ids`, since a completed execution is not
        // busy. Closing the channels lets `wait()` return; waiting on the
        // resulting `done` signal is what makes those tasks finish before the
        // test does, rather than being left running for nextest to find.
        // Subscribed before the drop on purpose: a `watch` receiver takes the
        // current value as already seen, so one created after the session
        // finished would wait for a second change that never comes.
        let mut dones: Vec<_> = executions.values().map(|active| active.done_rx()).collect();
        drop((_main_ch, _tenant_ch));
        for done in &mut dones {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(5), done.changed())
                    .await
                    .is_ok(),
                "a session whose channels are closed must finish"
            );
        }
    }

    /// Every spelling the runtime resolves must count as use — including an id
    /// prefix, which is the one that cannot be enumerated.
    ///
    /// `BoxManager::lookup_box` accepts an exact id, a name, or any
    /// unambiguous `id.starts_with(..)` prefix, and `RestBox` puts the caller's
    /// spelling straight into the path. A sweep that listed a box's spellings
    /// would have to produce every prefix — there are as many as the id is long
    /// — so it matches instead. Getting this wrong AutoStops a box that
    /// `boxlite --url … exec 01HJK4` is actively driving.
    #[test]
    fn a_spelling_resolves_the_way_the_runtime_resolves_it() {
        let id = "01HJK4TNRPQSXYZ8WM6NCVT9R5";
        let other = "01HJK9ZZZZZZZZZZZZZZZZZZZZ";
        let one = [(id.to_string(), Some("web".to_string()))];
        let two = [
            (id.to_string(), Some("web".to_string())),
            (other.to_string(), None),
        ];

        assert_eq!(box_named_by(id, &one), Some(id), "exact id");
        assert_eq!(box_named_by("web", &one), Some(id), "exact name");
        assert_eq!(box_named_by("01HJK4", &two), Some(id), "unique prefix");

        assert_eq!(box_named_by("", &one), None, "an empty spelling names none");
        assert_eq!(box_named_by("nope", &one), None, "names no box");
        assert_eq!(
            box_named_by("01HJK4TNRPQSXYZ8WM6NCVT9R5X", &one),
            None,
            "longer than the id is not a prefix of it"
        );

        // The one that matters: `BoxManager::lookup_box` errors on a prefix that
        // matches more than one box, so it must resolve to nothing here too.
        // Counting it would let one unauthenticated request stamp every box it
        // matched — `record_activity` stamps before any handler can reject it.
        assert_eq!(
            box_named_by("01HJK", &two),
            None,
            "an ambiguous prefix names no box"
        );
        assert_eq!(
            box_named_by("0", &two),
            None,
            "nor does a one-character one"
        );
    }

    /// A box driven by an id prefix must not look idle.
    ///
    /// The end-to-end companion to the truth table above: this is the spelling
    /// the sweep's old alias list could not represent at all.
    #[tokio::test]
    async fn a_box_driven_by_an_id_prefix_is_not_idle_under_its_full_id() {
        let map: RwLock<HashMap<String, Instant>> = RwLock::new(HashMap::new());
        let now = Instant::now();
        map.write().await.insert("01HJK4".to_string(), now);

        let idle = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            idle_or_seed(
                &map,
                "01HJK4TNRPQSXYZ8WM6NCVT9R5",
                &[("01HJK4TNRPQSXYZ8WM6NCVT9R5".to_string(), None)],
                now + std::time::Duration::from_secs(30),
            ),
        )
        .await
        .expect("reading must not deadlock");

        assert_eq!(
            idle,
            std::time::Duration::from_secs(30),
            "a prefix-addressed request is use of the box it resolves to"
        );
    }

    /// ...and pruning must not throw that stamp away either.
    #[tokio::test]
    async fn pruning_keeps_a_stamp_made_under_an_id_prefix() {
        let state = lifecycle_state();
        state.record_box_activity("01HJK4").await;

        state
            .retain_known_boxes(&[("01HJK4TNRPQSXYZ8WM6NCVT9R5".to_string(), None)])
            .await;

        assert!(
            state.last_activity.read().await.contains_key("01HJK4"),
            "pruning dropped a stamp that names a live box"
        );
    }

    #[tokio::test]
    async fn a_box_driven_by_name_is_not_idle_under_its_id() {
        // The middleware stamps the raw URL segment, and the contract lets that
        // be a name. Reading only the canonical id loses every request made by
        // name, and AutoStop then stops a box in continuous use.
        let map: RwLock<HashMap<String, Instant>> = RwLock::new(HashMap::new());
        let now = Instant::now();
        map.write().await.insert("web".to_string(), now);

        let idle = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            idle_or_seed(
                &map,
                "bx-abc123",
                &[("bx-abc123".to_string(), Some("web".to_string()))],
                now + std::time::Duration::from_secs(30),
            ),
        )
        .await
        .expect("reading must not deadlock");

        assert_eq!(idle, std::time::Duration::from_secs(30));
    }

    #[tokio::test]
    async fn the_idle_clock_drops_spellings_that_name_no_box() {
        // `record_activity` stamps before any handler can reject an unknown id,
        // and serve may run with no API key, so this is unauthenticated input.
        let state = lifecycle_state();
        state.record_box_activity("real").await;
        state.record_box_activity("never-existed").await;

        state
            .retain_known_boxes(&[("real".to_string(), None)])
            .await;

        let clocks = state.last_activity.read().await;
        assert!(clocks.contains_key("real"));
        assert!(
            !clocks.contains_key("never-existed"),
            "an id no box answers to must not hold a clock forever"
        );
    }

    #[tokio::test]
    async fn pruning_never_drops_a_deadline() {
        // `lifecycle` is written by create_box after its box may already be
        // missing from a caller's list_info snapshot. Pruning it against that
        // snapshot would drop the deadline permanently and silently.
        let state = lifecycle_state();
        state
            .set_lifecycle(
                "fresh",
                LifecyclePolicy {
                    auto_stop: 60,
                    auto_delete: 0,
                },
            )
            .await;

        state.retain_known_boxes(&[]).await;

        assert_eq!(state.lifecycle_of("fresh").await.auto_stop, 60);
    }

    // --- AutoResume gate ---

    /// The whole truth table, so inverting or dropping the rule fails here.
    #[test]
    fn autoresume_gates_only_a_stopped_box() {
        // `Stopped` is what AutoStop leaves behind, so it is the one state a
        // caller can be resumed *from*.
        assert!(autoresume_allows(BoxStatus::Stopped, true));
        assert!(
            !autoresume_allows(BoxStatus::Stopped, false),
            "a stopped box must refuse an implicit wake"
        );

        // Everything else is allowed whatever the flag says. Running/Paused are
        // already up. `Configured` has never run, and its first boot is not a
        // resume: gating it would make `run --url --no-auto-resume` refuse
        // itself, since `run` attaches before it starts the box.
        for status in [
            BoxStatus::Running,
            BoxStatus::Paused,
            BoxStatus::Configured,
            BoxStatus::Failed,
            BoxStatus::Stopping,
            BoxStatus::Unknown,
        ] {
            for auto_resume in [true, false] {
                assert!(
                    autoresume_allows(status, auto_resume),
                    "{status:?} with auto_resume={auto_resume} is not a resume"
                );
            }
        }
    }

    // --- Lifecycle sweep ---

    #[test]
    fn a_box_is_stopped_only_after_a_full_idle_window() {
        use std::time::Duration;
        let below = decide_lifecycle(
            BoxStatus::Running,
            60,
            0,
            Duration::from_secs(59),
            Duration::ZERO,
        );
        assert_eq!(below, LifecycleAction::Leave);

        // `>=`, so the boundary tick acts rather than waiting another 30s.
        let at = decide_lifecycle(
            BoxStatus::Running,
            60,
            0,
            Duration::from_secs(60),
            Duration::ZERO,
        );
        assert_eq!(at, LifecycleAction::Stop);
    }

    #[test]
    fn a_zero_window_disables_its_deadline() {
        use std::time::Duration;
        let huge = Duration::from_secs(86_400);
        assert_eq!(
            decide_lifecycle(BoxStatus::Running, 0, 0, huge, huge),
            LifecycleAction::Leave
        );
        assert_eq!(
            decide_lifecycle(BoxStatus::Stopped, 0, 0, huge, huge),
            LifecycleAction::Leave
        );
    }

    #[test]
    fn a_box_at_rest_is_deleted_on_its_own_deadline() {
        use std::time::Duration;
        // Anchored on the stop, not on last use: idle is zero here because the
        // box was busy right up to the moment it stopped, and it must still go.
        for status in [BoxStatus::Stopped, BoxStatus::Failed] {
            assert_eq!(
                decide_lifecycle(status, 0, 60, Duration::ZERO, Duration::from_secs(60)),
                LifecycleAction::Delete,
                "status {status:?}"
            );
        }
    }

    #[test]
    fn a_running_box_is_never_deleted_by_the_delete_deadline() {
        use std::time::Duration;
        // AutoDelete waits for the box to come to rest; a long-lived running
        // box must not be removed out from under its user.
        assert_eq!(
            decide_lifecycle(
                BoxStatus::Running,
                0,
                60,
                Duration::ZERO,
                Duration::from_secs(86_400)
            ),
            LifecycleAction::Leave
        );
    }

    #[test]
    fn box_scoped_paths_count_as_activity_except_the_pollers() {
        let post = Method::POST;
        assert_eq!(activity_box_id(&post, "/v1/boxes/abc/exec"), Some("abc"));
        assert_eq!(activity_box_id(&post, "/v1/boxes/abc/files"), Some("abc"));
        assert_eq!(activity_box_id(&post, "/v1/boxes/abc/attach"), Some("abc"));
        // `/start` counts: it is the one operation whose whole purpose is to
        // make a box usable again, so it must reset the window.
        assert_eq!(activity_box_id(&post, "/v1/boxes/abc/start"), Some("abc"));
        // A read of a sub-resource is still use of the box.
        assert_eq!(
            activity_box_id(&Method::GET, "/v1/boxes/abc/files"),
            Some("abc")
        );

        // Pollers must never hold a box open. A client watching `status` once a
        // second would otherwise reset the window before every sweep.
        assert_eq!(activity_box_id(&Method::GET, "/v1/boxes/abc"), None);
        assert_eq!(activity_box_id(&Method::HEAD, "/v1/boxes/abc"), None);
        assert_eq!(activity_box_id(&Method::DELETE, "/v1/boxes/abc"), None);
        assert_eq!(activity_box_id(&post, "/v1/boxes/abc/metrics"), None);
        assert_eq!(activity_box_id(&post, "/v1/boxes/abc/stop"), None);
        assert_eq!(activity_box_id(&post, "/v1/metrics"), None);
        assert_eq!(activity_box_id(&post, "/v1/boxes"), None);
    }

    #[tokio::test]
    async fn an_unseen_box_starts_its_window_now_rather_than_looking_idle() {
        // A box this `serve` has never seen reads as freshly used, not as idle
        // since boot — otherwise the first tick after a restart stops every
        // long-running box whose window is shorter than its uptime.
        //
        // Timeout-wrapped because the seeding path deadlocks rather than
        // returning a wrong answer if the read guard is held across the write
        // lock, and a hanging test stalls CI instead of failing it.
        let map: RwLock<HashMap<String, Instant>> = RwLock::new(HashMap::new());
        let now = Instant::now();
        let budget = std::time::Duration::from_secs(3);

        let first = tokio::time::timeout(
            budget,
            idle_or_seed(&map, "b", &[("b".to_string(), None)], now),
        )
        .await
        .expect("seeding must not deadlock");
        assert_eq!(first, std::time::Duration::ZERO);

        let later = now + std::time::Duration::from_secs(600);
        let second = tokio::time::timeout(
            budget,
            idle_or_seed(&map, "b", &[("b".to_string(), None)], later),
        )
        .await
        .expect("reading a seeded box must not deadlock");
        assert_eq!(second, std::time::Duration::from_secs(600));
    }

    #[test]
    fn build_box_options_legacy_network_defaults_inbound_to_enabled() {
        // Legacy flat `network` never carried an inbound concept — it
        // predates the outbound/inbound split — so inbound falls back to
        // its default (Enabled/public) regardless of outbound mode.
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{
                "image": "alpine:latest",
                "network": {
                    "mode": "enabled"
                }
            }"#,
        )
        .expect("legacy flat body must deserialize");
        let opts = build_box_options(&req).expect("build");
        assert!(
            matches!(opts.inbound_network, NetworkSpec::Enabled { ref allow_net } if allow_net.is_empty())
        );
    }

    #[test]
    fn build_box_options_accepts_nested_network_spec() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{
                "image": "alpine:latest",
                "network": {
                    "outbound": {
                        "mode": "enabled",
                        "allow_net": ["api.openai.com"]
                    },
                    "inbound": {
                        "mode": "disabled"
                    }
                }
            }"#,
        )
        .expect("nested network body must deserialize");
        let opts = build_box_options(&req).expect("build");
        match opts.network {
            NetworkSpec::Enabled { allow_net } => {
                assert_eq!(allow_net, vec!["api.openai.com"]);
            }
            NetworkSpec::Disabled => panic!("network should be enabled"),
        }
        assert!(matches!(opts.inbound_network, NetworkSpec::Disabled));
    }

    #[test]
    fn build_box_options_rejects_mixed_legacy_and_nested_network_spec() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{
                "image": "alpine:latest",
                "network": {
                    "mode": "enabled",
                    "outbound": {
                        "mode": "enabled",
                        "allow_net": ["api.openai.com"]
                    }
                }
            }"#,
        )
        .expect("mixed network body still deserializes for compatibility validation");
        let err = build_box_options(&req).expect_err("mixed network body must fail");
        assert!(
            matches!(err, boxlite::BoxliteError::InvalidArgument(ref msg) if msg.contains("either nested")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_box_options_rejects_empty_legacy_allow_net_mixed_with_nested_network_spec() {
        let req: super::types::CreateBoxRequest = serde_json::from_str(
            r#"{
                "image": "alpine:latest",
                "network": {
                    "allow_net": [],
                    "outbound": {
                        "mode": "enabled"
                    }
                }
            }"#,
        )
        .expect("mixed network body still deserializes for compatibility validation");
        let err = build_box_options(&req).expect_err("mixed network body must fail");
        assert!(
            matches!(err, boxlite::BoxliteError::InvalidArgument(ref msg) if msg.contains("either nested")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_box_options_empty_body_lands_on_server_default_security() {
        // Bog-standard REST body. Server resolves security from its
        // own default; on Linux/macOS that's jailer-on (the standard
        // preset, post-flip).
        let json = r#"{"image": "alpine:latest"}"#;
        let req: super::types::CreateBoxRequest =
            serde_json::from_str(json).expect("body must deserialize");
        let opts = build_box_options(&req).expect("build_box_options");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(
            opts.advanced.security.jailer_enabled,
            "server default must be sandbox-on after the flip"
        );
    }

    #[test]
    fn create_box_request_rejects_client_supplied_security_preset() {
        // A malicious or careless client sends `security: "development"`
        // hoping to disable the jailer. `deny_unknown_fields` turns
        // that into a hard deserialize error, which the REST layer
        // surfaces as a 400 — there is no quiet fall-through.
        let json = r#"{"image": "alpine:latest", "security": "development"}"#;
        let msg = match serde_json::from_str::<super::types::CreateBoxRequest>(json) {
            Ok(_) => panic!("`security` must be rejected at deserialize"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("unknown field") && msg.contains("security"),
            "expected deny-unknown-fields rejection mentioning `security`; got {msg}"
        );
    }

    #[test]
    fn create_box_request_rejects_client_supplied_security_settings() {
        // Same shape as the previous test but with a `security_settings`
        // struct. Also blocked at deserialize.
        let json = r#"{
            "image": "alpine:latest",
            "security_settings": {
                "jailer_enabled":  false,
                "seccomp_enabled": false,
                "uid": null,
                "gid": null,
                "new_pid_ns": false,
                "new_net_ns": false,
                "chroot_base": "/srv/boxlite",
                "chroot_enabled": false,
                "close_fds": false,
                "sanitize_env": false,
                "env_allowlist": [],
                "resource_limits": {},
                "sandbox_profile": null,
                "network_enabled": true
            }
        }"#;
        let msg = match serde_json::from_str::<super::types::CreateBoxRequest>(json) {
            Ok(_) => panic!("`security_settings` must be rejected at deserialize"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("unknown field") && msg.contains("security_settings"),
            "expected deny-unknown-fields rejection mentioning `security_settings`; got {msg}"
        );
    }

    fn uploaded_v3_archive(box_options: serde_json::Value) -> Vec<u8> {
        let backing_path = b"/server/path/must-not-be-read";
        let mut disk = vec![0_u8; 1024];
        disk[0..4].copy_from_slice(&0x5146_49fbu32.to_be_bytes());
        disk[4..8].copy_from_slice(&3_u32.to_be_bytes());
        disk[8..16].copy_from_slice(&512_u64.to_be_bytes());
        disk[16..20].copy_from_slice(&(backing_path.len() as u32).to_be_bytes());
        disk[512..512 + backing_path.len()].copy_from_slice(backing_path);

        let manifest = serde_json::json!({
            "version": 3,
            "box_name": null,
            "image": "alpine:latest",
            "box_options": box_options,
            "guest_disk_checksum": "",
            "container_disk_checksum": "",
            "exported_at": "2026-07-26T00:00:00Z"
        });
        let manifest = serde_json::to_vec(&manifest).expect("serialize manifest");
        let mut archive = tar::Builder::new(Vec::new());

        for (name, bytes) in [
            ("manifest.json", manifest.as_slice()),
            ("disk.qcow2", disk.as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o600);
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            archive
                .append_data(&mut header, name, bytes)
                .expect("append archive entry");
        }

        archive.finish().expect("finish archive");
        archive.into_inner().expect("archive bytes")
    }

    async fn upload_v3_archive(
        state: Arc<AppState>,
        box_options: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = handlers::advanced::import_box(
            State(state),
            axum::extract::Query(types::ImportQuery { name: None }),
            axum::body::Bytes::from(uploaded_v3_archive(box_options)),
        )
        .await;
        let status = response.status();
        let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let body = serde_json::from_slice(&response_body).expect("JSON response");
        (status, body)
    }

    async fn assert_uploaded_archive_rejected_before_provisioning(
        box_options: serde_json::Value,
        expected_message: &str,
    ) {
        let home = tempfile::tempdir().expect("runtime home");
        let runtime = BoxliteRuntime::new(boxlite::BoxliteOptions {
            home_dir: home.path().join("boxlite"),
            ..Default::default()
        })
        .expect("local runtime");
        let state = Arc::new(AppState {
            runtime: runtime.clone(),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        let (control_status, control_error) =
            upload_v3_archive(state.clone(), serde_json::json!({})).await;
        assert_eq!(control_status, StatusCode::CONFLICT);
        assert_eq!(control_error["error"]["type"], "InvalidStateError");
        assert_eq!(control_error["error"]["code"], "invalid_state");
        assert!(
            control_error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("backing file reference")),
            "control upload must reach disk validation: {control_error}"
        );

        let (status, error) = upload_v3_archive(state, box_options).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["type"], "UnsupportedError");
        assert_eq!(error["error"]["code"], "unsupported");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(expected_message))
        );
        assert!(
            runtime.list_info().await.expect("list boxes").is_empty(),
            "rejected upload must not provision a box"
        );
        runtime.shutdown(Some(1)).await.expect("shutdown runtime");
    }

    #[tokio::test]
    async fn serve_import_rejects_nested_virtualization_archive_before_provisioning() {
        assert_uploaded_archive_rejected_before_provisioning(
            serde_json::json!({"advanced": {"nested_virtualization": true}}),
            "nested virtualization",
        )
        .await;
    }

    #[tokio::test]
    async fn serve_import_rejects_host_volume_archive_before_provisioning() {
        assert_uploaded_archive_rejected_before_provisioning(
            serde_json::json!({
                "volumes": [{
                    "host_path": "/",
                    "guest_path": "/host",
                    "read_only": false
                }]
            }),
            "volume mounts",
        )
        .await;
    }

    /// The managed-volume shape is refused by the same gate, and reaches it
    /// through the same deserialization — an archive naming someone else's
    /// volume must not provision a box either.
    #[tokio::test]
    async fn serve_import_rejects_managed_volume_archive_before_provisioning() {
        assert_uploaded_archive_rejected_before_provisioning(
            serde_json::json!({
                "volumes": [{
                    "managed_volume": "someone-elses-data",
                    "guest_path": "/data",
                    "read_only": false
                }]
            }),
            "volume mounts",
        )
        .await;
    }

    /// Build an `ActiveExecution` backed by a stub `Execution` whose
    /// stdout/stderr/result channels we control from the test.
    fn make_test_active() -> (
        Arc<ActiveExecution>,
        tokio::sync::mpsc::UnboundedSender<String>, // stdout driver
        tokio::sync::mpsc::UnboundedSender<String>, // stderr driver
        tokio::sync::mpsc::UnboundedSender<boxlite::ExecResult>, // result driver
    ) {
        let (exec, stdout_tx, stderr_tx, _stdin_rx, result_tx) =
            boxlite::Execution::stub("test-exec");
        let active = ActiveExecution::new("test-box".to_string(), SessionKind::Exec, exec, None);
        (active, stdout_tx, stderr_tx, result_tx)
    }

    // ---------------------------------------------------------------
    // Main session (container init) — `GET /boxes/{id}/attach`
    // ---------------------------------------------------------------

    /// Channel handles that keep a stub `Execution` alive. Dropping them
    /// closes the result channel, which the wait task reads as "process
    /// exited"; a main session under test must stay running.
    #[allow(dead_code)]
    struct StubChannels(
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        tokio::sync::mpsc::UnboundedSender<boxlite::ExecResult>,
    );

    fn stub_execution(id: &str) -> (Execution, StubChannels) {
        let (exec, stdout_tx, stderr_tx, stdin_rx, result_tx) = boxlite::Execution::stub(id);
        (
            exec,
            StubChannels(stdout_tx, stderr_tx, stdin_rx, result_tx),
        )
    }

    // A second `GET /boxes/{id}/attach` must reuse the registered main
    // session, never open a second one. The guest binds init's stdout to
    // the first Attach RPC and rejects later ones, so a second
    // `LiteBox::attach()` would return an Execution that never streams —
    // and would hand a second client a live attach slot. The registry
    // entry is the record that the session is already open, so the second
    // call must find it and get refused by `mark_connected()` (the 409).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_attach_reuses_main_session_and_never_reopens_it() {
        let mut executions: HashMap<String, Arc<ActiveExecution>> = HashMap::new();
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // A tenant exec on the same box must not be mistaken for the main
        // session — it is addressed by its own id and has its own slot.
        let (tenant, _tenant_channels) = stub_execution("exec-1");
        executions.insert(
            "exec-1".to_string(),
            ActiveExecution::new("box1".to_string(), SessionKind::Exec, tenant, None),
        );

        let (init, _init_channels) = stub_execution("container-1");
        let mut init = Some(init);
        let first = register_main_session(&mut executions, "box1", || {
            opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let init = init.take().expect("opener runs once");
            async move { Ok(init) }
        })
        .await
        .expect("first attach opens the main session");

        // Second attach: the opener here would produce a *different*
        // session. If it ever runs, we have opened a second guest stream.
        let (decoy, _decoy_channels) = stub_execution("container-DECOY");
        let mut decoy = Some(decoy);
        let second = register_main_session(&mut executions, "box1", || {
            opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let decoy = decoy.take().expect("opener runs once");
            async move { Ok(decoy) }
        })
        .await
        .expect("second attach resolves the main session");

        assert_eq!(
            opens.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second attach must reuse the registered main session, not open another one",
        );
        assert!(
            Arc::ptr_eq(&first, &second),
            "both attaches must resolve to the same ActiveExecution",
        );
        assert_eq!(
            second.execution().id(),
            "container-1",
            "the main session keeps the container id it was opened with",
        );
        assert!(
            executions.contains_key("container-1"),
            "the main session is registered under its container id, alongside tenant execs: {:?}",
            executions.keys().collect::<Vec<_>>(),
        );

        // The single-attach claim is what turns the reused entry into a
        // 409 for the second client, exactly as for an exec session.
        assert!(first.mark_connected().await, "first client claims the slot");
        assert!(
            !second.mark_connected().await,
            "a second client on an attached main session must be refused (409)",
        );
    }

    // The main session is found by box id — the container id is not
    // knowable to the caller. Exec sessions and other boxes' sessions must
    // never answer that lookup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_main_session_matches_only_the_boxs_init_session() {
        let mut executions: HashMap<String, Arc<ActiveExecution>> = HashMap::new();

        let (exec, _exec_channels) = stub_execution("exec-1");
        executions.insert(
            "exec-1".to_string(),
            ActiveExecution::new("box1".to_string(), SessionKind::Exec, exec, None),
        );
        let (other_init, _other_channels) = stub_execution("container-2");
        executions.insert(
            "container-2".to_string(),
            ActiveExecution::new("box2".to_string(), SessionKind::Main, other_init, None),
        );

        assert!(
            find_main_session(&executions, "box1").is_none(),
            "box1 has only an exec session — attaching must open its main session, not adopt the exec",
        );

        let (init, _init_channels) = stub_execution("container-1");
        executions.insert(
            "container-1".to_string(),
            ActiveExecution::new("box1".to_string(), SessionKind::Main, init, None),
        );

        let found = find_main_session(&executions, "box1").expect("box1 main session");
        assert_eq!(
            found.execution().id(),
            "container-1",
            "must not return box2's main session",
        );
    }

    const STUB_BOX_ID: &str = "01HJK4TNRPQSXYZ8WM6NCVT9R5";
    /// The same box's user-defined name. The wire contract lets a client address
    /// a box by either, so both spellings belong in these tests.
    const STUB_BOX_NAME: &str = "swept-box";

    /// A running stub upstream, plus counters for the writes it is asked to make.
    struct StubUpstream {
        url: String,
        task: tokio::task::JoinHandle<()>,
        stops: Arc<std::sync::atomic::AtomicUsize>,
        deletes: Arc<std::sync::atomic::AtomicUsize>,
        execs: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl StubUpstream {
        fn stops(&self) -> usize {
            self.stops.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn deletes(&self) -> usize {
            self.deletes.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// How many times the guest was actually asked to run something.
        ///
        /// A gate that let a request through is only distinguishable from one
        /// the request never reached by whether the runtime call happened.
        fn execs(&self) -> usize {
            self.execs.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// An upstream that answers the box reads with one canned box, so a handler
    /// can be driven end to end without a VM.
    ///
    /// The box it reports is shaped like one created through `serve`: both
    /// deadlines are zero, because `build_box_options` withheld them from the
    /// runtime. `auto_resume` is a parameter because the resolvers disagree
    /// about it — one consults it, one consults the held AutoStop deadline —
    /// and that disagreement is what lets a test tell them apart.
    async fn stub_upstream(status: &'static str, auto_resume: bool) -> StubUpstream {
        fn canned_box(status: &str, auto_resume: bool) -> serde_json::Value {
            serde_json::json!({
                "box_id": STUB_BOX_ID,
                "name": STUB_BOX_NAME,
                "status": status,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "pid": null,
                "image": "alpine:latest",
                "cpus": 1,
                "memory_mib": 512,
                "auto_stop": 0,
                "auto_delete": 0,
                "auto_resume": auto_resume,
            })
        }

        let deletes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/v1/boxes/{id}/exec",
                axum::routing::post({
                    let execs = Arc::clone(&execs);
                    move |axum::extract::Path(id): axum::extract::Path<String>| {
                        let execs = Arc::clone(&execs);
                        async move {
                            if id == STUB_BOX_ID || id == STUB_BOX_NAME {
                                execs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            // The reply shape does not matter: the counter is the
                            // whole point, and every caller here is proving only
                            // that the request got this far.
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/boxes",
                axum::routing::get(move || async move {
                    Json(serde_json::json!({ "boxes": [canned_box(status, auto_resume)] }))
                })
                // Creation reports the box back with both deadlines at zero,
                // exactly as the real runtime does once `build_box_options` has
                // withheld them — so a test can tell a stored policy from an
                // echoed one.
                .post(move |_body: Json<serde_json::Value>| async move {
                    (StatusCode::CREATED, Json(canned_box(status, auto_resume)))
                }),
            )
            .route(
                "/v1/boxes/{id}/stop",
                axum::routing::post({
                    let stops = Arc::clone(&stops);
                    move |axum::extract::Path(id): axum::extract::Path<String>| {
                        let stops = Arc::clone(&stops);
                        async move {
                            // Gated on the id like the sibling delete, so the
                            // counter says which box was stopped and not merely
                            // that something was.
                            if id == STUB_BOX_ID || id == STUB_BOX_NAME {
                                stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                Json(canned_box("stopped", auto_resume)).into_response()
                            } else {
                                StatusCode::NOT_FOUND.into_response()
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1/boxes/{id}",
                axum::routing::get(
                    // Answers to either spelling with the same box, reporting its
                    // canonical id — which is what the real runtime does, since
                    // it resolves a name before returning info. A stub that
                    // echoed the requested spelling back as `box_id` would hide
                    // exactly the id-vs-name bugs these tests exist to catch.
                    move |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        if id == STUB_BOX_ID || id == STUB_BOX_NAME {
                            Json(canned_box(status, auto_resume)).into_response()
                        } else {
                            StatusCode::NOT_FOUND.into_response()
                        }
                    },
                )
                .delete({
                    let deletes = Arc::clone(&deletes);
                    move |axum::extract::Path(id): axum::extract::Path<String>| {
                        let deletes = Arc::clone(&deletes);
                        async move {
                            if id == STUB_BOX_ID || id == STUB_BOX_NAME {
                                deletes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                StatusCode::NO_CONTENT.into_response()
                            } else {
                                StatusCode::NOT_FOUND.into_response()
                            }
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        StubUpstream {
            url: format!("http://127.0.0.1:{port}"),
            task,
            stops,
            deletes,
            execs,
        }
    }

    /// Serve `state` on an ephemeral port and return its base URL.
    async fn serve_router(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(state)).await;
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    /// Reading a box back over HTTP must report the deadlines `serve` holds.
    ///
    /// `box_response` being correct is not enough — the defect this guards is a
    /// handler that does not call it. Rust lets these handlers reach
    /// `box_info_to_response` directly from their parent module, so rewiring one
    /// back to it compiles clean; only driving the real route catches it.
    ///
    /// Both read shapes are driven: the single value and the list's loop. The
    /// remaining sites (`start`, `stop`, `clone`, `import`) build their response
    /// from the same single value, but only after an operation a stub upstream
    /// cannot stand in for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reading_a_box_over_http_reports_the_deadlines_serve_holds() {
        let upstream = stub_upstream("stopped", true).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 900,
                    auto_delete: 604_800,
                },
            )
            .await;
        let (base, server) = serve_router(Arc::clone(&state)).await;
        let client = reqwest::Client::new();

        let one: serde_json::Value = client
            .get(format!("{base}/v1/boxes/{STUB_BOX_ID}"))
            .send()
            .await
            .expect("request must reach the server")
            .json()
            .await
            .expect("a box body");
        assert_eq!(
            one["auto_stop"], 900,
            "GET must report the AutoStop deadline serve is sweeping, not the runtime's zero"
        );
        assert_eq!(
            one["auto_delete"], 604_800,
            "GET must report the AutoDelete deadline serve is sweeping, not the runtime's zero"
        );

        let listed: serde_json::Value = client
            .get(format!("{base}/v1/boxes"))
            .send()
            .await
            .expect("request must reach the server")
            .json()
            .await
            .expect("a list body");
        assert_eq!(
            listed["boxes"][0]["auto_stop"], 900,
            "the list must report the same deadlines the single read does"
        );
        assert_eq!(listed["boxes"][0]["auto_delete"], 604_800);

        server.abort();
        upstream.task.abort();
    }

    /// Removing a box by name must forget the deadline filed under its id.
    ///
    /// `retain_known_boxes` prunes the idle clock but deliberately never touches
    /// the deadline map, so an entry missed here is missed for the life of the
    /// process. Driven by name specifically: forgetting the spelling the request
    /// used is what leaves the id-keyed entry behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_a_box_by_name_forgets_the_deadline_filed_under_its_id() {
        let upstream = stub_upstream("stopped", true).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 900,
                    auto_delete: 604_800,
                },
            )
            .await;
        let (base, server) = serve_router(Arc::clone(&state)).await;

        let response = reqwest::Client::new()
            .delete(format!("{base}/v1/boxes/{STUB_BOX_NAME}"))
            .send()
            .await
            .expect("request must reach the server");
        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "the box must actually have been removed, or the forget is untested"
        );

        assert_eq!(
            upstream.deletes(),
            1,
            "the box must have been removed upstream, not just forgotten locally"
        );
        assert!(
            !state.lifecycle.read().await.contains_key(STUB_BOX_ID),
            "the deadline outlived the box it belonged to"
        );
        server.abort();
        upstream.task.abort();
    }

    /// The sweep itself must reach the runtime, not just decide correctly.
    ///
    /// `decide_lifecycle`, the busy set, the aliasing and the idle seeding are
    /// each covered in isolation; this is the loop that wires them to
    /// `list_info` and `remove`. Without it, every part could be right while
    /// nothing is ever actually swept.
    ///
    /// This drives the AutoDelete arm; `the_sweep_stops_a_box_idle_past_its_
    /// window` drives the Stop arm. `last_updated` on the canned box is long
    /// past, so the deadline below is comfortably elapsed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_removes_a_box_whose_delete_deadline_has_passed() {
        let upstream = stub_upstream("stopped", true).await;
        let state = AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        };
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 0,
                    auto_delete: 60,
                },
            )
            .await;

        run_lifecycle_once(&state, Instant::now()).await;

        assert_eq!(
            upstream.deletes(),
            1,
            "a box at rest past its AutoDelete window must actually be removed"
        );
        assert!(
            !state.lifecycle.read().await.contains_key(STUB_BOX_ID),
            "a removed box must not keep its deadline"
        );
        upstream.task.abort();
    }

    /// Creating a box over the wire must actually file its deadlines.
    ///
    /// `create_box`'s `set_lifecycle` is the only production write to the
    /// lifecycle map — every other caller is a test seeding it by hand. Delete
    /// that one line and the whole feature goes inert while the suite stays
    /// green: no box acquires a deadline, every read reports zero, and the sweep
    /// never acts. Nothing else here can catch that, so this drives the real
    /// route and checks both the response and the map behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn creating_a_box_over_http_files_the_deadlines_it_asked_for() {
        let upstream = stub_upstream("running", true).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        let (base, server) = serve_router(Arc::clone(&state)).await;

        let created: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/v1/boxes"))
            .json(&serde_json::json!({
                "image": "alpine:latest",
                "auto_stop": 900,
                "auto_delete": 604_800,
            }))
            .send()
            .await
            .expect("request must reach the server")
            .json()
            .await
            .expect("a box body");

        assert_eq!(
            created["auto_stop"], 900,
            "the create response must echo the policy, not the runtime's zero"
        );
        assert_eq!(created["auto_delete"], 604_800);
        assert_eq!(
            state.lifecycle.read().await.get(STUB_BOX_ID).copied(),
            Some(LifecyclePolicy {
                auto_stop: 900,
                auto_delete: 604_800,
            }),
            "the sweep reads this map; a policy that never lands here is never enforced"
        );

        server.abort();
        upstream.task.abort();
    }

    /// A box addressed by a name that needs escaping must still count as used.
    ///
    /// The idle clock is written from the raw request path and read from the
    /// decoded `BoxInfo.name`, so without decoding at the write site the two
    /// spellings never meet and a box in continuous use is swept as idle. The
    /// contract's `name` pattern forbids a name that needs escaping, but `serve`
    /// does not enforce it, so such a name reaches this code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn activity_on_an_escaped_name_lands_under_the_name_the_sweep_reads() {
        let upstream = stub_upstream("running", true).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        let (base, server) = serve_router(Arc::clone(&state)).await;

        let _ = reqwest::Client::new()
            .post(format!("{base}/v1/boxes/my%20box/exec"))
            .json(&serde_json::json!({"command": "echo"}))
            .send()
            .await
            .expect("request must reach the server");

        // A legal name, escaped anyway. Percent-encoding an unreserved character
        // is allowed, so this case does not depend on the contract's `name`
        // pattern going unenforced — it would still arrive if that gap closed.
        let _ = reqwest::Client::new()
            .post(format!("{base}/v1/boxes/swept%2Dbox/exec"))
            .json(&serde_json::json!({"command": "echo"}))
            .send()
            .await
            .expect("request must reach the server");

        let clocks = state.last_activity.read().await;
        assert!(
            clocks.contains_key("my box"),
            "the stamp must be filed under the decoded name the sweep looks for, \
             not the escaped spelling: {:?}",
            clocks.keys().collect::<Vec<_>>()
        );
        assert!(
            clocks.contains_key(STUB_BOX_NAME),
            "an escaped unreserved character must decode too: {:?}",
            clocks.keys().collect::<Vec<_>>()
        );

        drop(clocks);
        server.abort();
        upstream.task.abort();
    }

    /// AutoResume must be enforced by the routes, not merely decided correctly.
    ///
    /// `autoresume_allows` is exhaustively unit-tested, but that says nothing
    /// about whether any handler consults it: `get_or_resume_box` and
    /// `get_or_fetch_box` have identical signatures, so rewiring a route to the
    /// ungated one compiles clean and every predicate test stays green. Only
    /// driving the route catches it. The body is asserted too, because a 409
    /// alone would not distinguish this gate from the metrics one.
    ///
    /// All four call sites are driven, because each handler resolves for itself
    /// and rewiring any one of them alone is invisible to the others' tests:
    /// `exec`, both halves of `files` (upload and download are separate
    /// handlers on one route), and `attach`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_box_with_autoresume_off_refuses_the_routes_that_would_wake_it() {
        let upstream = stub_upstream("stopped", false).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        let (base, server) = serve_router(Arc::clone(&state)).await;
        let client = reqwest::Client::new();

        let exec = client
            .post(format!("{base}/v1/boxes/{STUB_BOX_ID}/exec"))
            .json(&serde_json::json!({"command": "echo"}))
            .send()
            .await
            .expect("request must reach the server");
        assert_eq!(
            exec.status().as_u16(),
            409,
            "exec must not implicitly wake a box whose caller disabled AutoResume"
        );
        let body = exec.text().await.expect("an error body");
        assert!(
            body.contains("has AutoResume disabled"),
            "the refusal must be the AutoResume gate, not another 409: {body}"
        );

        let download = client
            .get(format!("{base}/v1/boxes/{STUB_BOX_ID}/files?path=/tmp"))
            .send()
            .await
            .expect("request must reach the server");
        assert_eq!(
            download.status().as_u16(),
            409,
            "reading files must be gated too; it drives the guest just as exec does"
        );
        assert!(
            download
                .text()
                .await
                .expect("an error body")
                .contains("has AutoResume disabled"),
            "the refusal must be the AutoResume gate, not another 409"
        );

        let upload = client
            .put(format!("{base}/v1/boxes/{STUB_BOX_ID}/files?path=/tmp/x"))
            .body(Vec::new())
            .send()
            .await
            .expect("request must reach the server");
        assert_eq!(
            upload.status().as_u16(),
            409,
            "writing files is a separate handler and resolves for itself"
        );
        assert!(
            upload
                .text()
                .await
                .expect("an error body")
                .contains("has AutoResume disabled"),
            "the refusal must be the AutoResume gate, not another 409"
        );

        // Attach goes through a fourth resolver, and its `WebSocketUpgrade`
        // extractor rejects a plain GET before the handler runs — so the refusal
        // is only observable through a real handshake.
        let ws_url = base.replacen("http://", "ws://", 1);
        let refused =
            tokio_tungstenite::connect_async(format!("{ws_url}/v1/boxes/{STUB_BOX_ID}/attach"))
                .await
                .expect_err("the handshake must be refused, not completed");
        match refused {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(
                    response.status().as_u16(),
                    409,
                    "attach must not implicitly wake the box either"
                );
                // tungstenite fills this from whatever it had already buffered
                // past the response headers, so in principle a split write
                // would leave it empty. Asserted unconditionally anyway: the
                // body does arrive over loopback, and tolerating an empty one
                // would quietly reduce this to a status-only check the day it
                // stopped. A flake here is worth more than a silent downgrade.
                let body = String::from_utf8_lossy(response.body().as_deref().unwrap_or_default())
                    .into_owned();
                assert!(
                    body.contains("has AutoResume disabled"),
                    "the refusal must be the AutoResume gate, not the already-attached 409: {body}"
                );
            }
            other => panic!("expected an HTTP refusal, got {other:?}"),
        }

        server.abort();
        upstream.task.abort();
    }

    /// ...and must not refuse when the caller left AutoResume on.
    ///
    /// Without this the test above passes just as well against a route that
    /// answers 409 unconditionally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_box_with_autoresume_on_is_not_refused() {
        let upstream = stub_upstream("stopped", true).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        let (base, server) = serve_router(Arc::clone(&state)).await;

        let exec = reqwest::Client::new()
            .post(format!("{base}/v1/boxes/{STUB_BOX_ID}/exec"))
            .json(&serde_json::json!({"command": "echo"}))
            .send()
            .await
            .expect("request must reach the server");
        assert_ne!(
            exec.status().as_u16(),
            409,
            "AutoResume was enabled; the wake must not have been refused"
        );
        // A non-409 alone would also be satisfied by a request that never
        // reached the gate at all. What proves it was let *through* is that the
        // runtime went on to ask the guest to run something.
        assert_eq!(
            upstream.execs(),
            1,
            "the request must have reached the guest, not merely avoided a 409"
        );

        server.abort();
        upstream.task.abort();
    }

    /// The Stop arm, likewise driven end to end.
    ///
    /// This one goes through `runtime.get` and `LiteBox::stop` rather than a
    /// bare delete, so it covers the longer of the two paths out of
    /// `decide_lifecycle`. The box has never been seen by this server, so
    /// `idle_or_seed` would normally seed it at zero and spare it — the clock is
    /// pre-stamped far enough back to put it past its window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_stops_a_box_idle_past_its_window() {
        let upstream = stub_upstream("running", true).await;
        let state = AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        };
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 60,
                    auto_delete: 0,
                },
            )
            .await;
        let now = Instant::now();
        state
            .last_activity
            .write()
            .await
            .insert(STUB_BOX_ID.to_string(), now - Duration::from_secs(600));

        run_lifecycle_once(&state, now).await;

        assert_eq!(
            upstream.stops(),
            1,
            "a running box idle past its AutoStop window must actually be stopped"
        );
        upstream.task.abort();
    }

    /// ...and must leave a box alone that is still inside its window.
    ///
    /// Without this the test above passes just as well against a sweep that
    /// stops unconditionally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_leaves_a_box_still_inside_its_idle_window() {
        let upstream = stub_upstream("running", true).await;
        let state = AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        };
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 600,
                    auto_delete: 0,
                },
            )
            .await;
        let now = Instant::now();
        state
            .last_activity
            .write()
            .await
            .insert(STUB_BOX_ID.to_string(), now - Duration::from_secs(60));

        run_lifecycle_once(&state, now).await;

        assert_eq!(
            upstream.stops(),
            0,
            "the sweep stopped a box that had been used inside its window"
        );
        upstream.task.abort();
    }

    /// ...and must leave a box alone whose delete deadline has not passed.
    ///
    /// Without this the delete test passes just as well against a sweep that
    /// removes unconditionally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_leaves_a_box_whose_deadline_has_not_passed() {
        let upstream = stub_upstream("stopped", true).await;
        let state = AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        };
        // The canned box's `last_updated` is a fixed date in the past, so its
        // time at rest grows as the calendar does. A century-long window
        // out-reaches that for the life of this test.
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 0,
                    auto_delete: 60 * 60 * 24 * 365 * 100,
                },
            )
            .await;

        run_lifecycle_once(&state, Instant::now()).await;

        assert_eq!(
            upstream.deletes(),
            0,
            "the sweep removed a box that was still inside its AutoDelete window"
        );
        assert!(
            state.lifecycle.read().await.contains_key(STUB_BOX_ID),
            "a box that was not removed must keep its deadline"
        );
        upstream.task.abort();
    }

    /// A metrics scrape must be refused rather than wake a box the sweep would
    /// stop again — checked through the real route, not the predicate.
    ///
    /// The stub box has `auto_resume: true`, so the AutoResume resolver would
    /// let this through; only the metrics resolver refuses it. That is exactly
    /// the rewiring that would silently restore the thrash loop.
    ///
    /// Both spellings are driven. The deadline is filed under the box's id, but
    /// the path segment may be a user-defined name — so a lookup keyed off the
    /// segment reads no deadline for the name and wakes the box, and only the
    /// name half of this test can see that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scraping_metrics_will_not_wake_a_box_the_sweep_would_stop() {
        let upstream = stub_upstream("stopped", true).await;
        let state = Arc::new(AppState {
            runtime: BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(upstream.url.clone()))
                .expect("rest runtime"),
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });
        state
            .set_lifecycle(
                STUB_BOX_ID,
                LifecyclePolicy {
                    auto_stop: 900,
                    auto_delete: 604_800,
                },
            )
            .await;
        let (base, server) = serve_router(Arc::clone(&state)).await;

        for spelling in [STUB_BOX_ID, STUB_BOX_NAME] {
            let response = reqwest::Client::new()
                .get(format!("{base}/v1/boxes/{spelling}/metrics"))
                .send()
                .await
                .expect("request must reach the server");
            assert_eq!(
                response.status().as_u16(),
                409,
                "a scrape addressed by {spelling:?} must be refused, not answered by booting the box"
            );
            let body = response.text().await.expect("an error body");
            assert!(
                body.contains("a metrics scrape does not start a box"),
                "the refusal must be the metrics rule, not the AutoResume gate: {body}"
            );

            assert!(
                !state.last_activity.read().await.contains_key(spelling),
                "a scrape must not stamp the idle clock either"
            );
        }
        server.abort();
        upstream.task.abort();
    }

    /// An unauthenticated request must not stamp a box's idle clock.
    ///
    /// This rests entirely on layer order: axum applies `.layer()` calls in
    /// reverse, so `require_api_key` is added last to end up outermost and
    /// short-circuit before `record_activity` runs. Swapping the two adjacent
    /// calls inverts it silently, and any caller could then hold someone else's
    /// box open past its AutoStop window.
    #[tokio::test]
    async fn a_rejected_request_does_not_stamp_the_idle_clock() {
        let runtime = BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .expect("rest runtime");
        let state = Arc::new(AppState {
            runtime,
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: Some("expected-key".to_string()),
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let _ = axum::serve(listener, build_router(state)).await;
            })
        };

        let unauthorized = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/boxes/victim/exec"))
            .json(&serde_json::json!({"command": "echo"}))
            .send()
            .await
            .expect("request must reach the server");
        assert_eq!(unauthorized.status().as_u16(), 401);

        assert!(
            !state.last_activity.read().await.contains_key("victim"),
            "a 401'd request must not have reset the box's idle window"
        );
        server.abort();
    }

    // The container-attach route must be registered at the path the client
    // builds (`RestBox::attach` → `/v1/boxes/{id}/attach`). A method it
    // does not serve proves the path matched (405); an adjacent path that
    // was never registered proves the opposite (404). Neither reaches a
    // handler, so no box or VM is touched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn container_attach_route_is_registered() {
        // A REST-backed runtime keeps AppState cheap: no local runtime
        // dirs, no embedded-runtime extraction. Routing is decided before
        // any handler runs, so the runtime is never called.
        let runtime = BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .expect("rest runtime");
        let state = Arc::new(AppState {
            runtime,
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(state)).await;
        });

        let http = reqwest::Client::new();
        let attached = http
            .post(format!("http://127.0.0.1:{port}/v1/boxes/box1/attach"))
            .send()
            .await
            .expect("POST /attach");
        assert_eq!(
            attached.status().as_u16(),
            405,
            "GET /v1/boxes/{{box_id}}/attach must be registered (405 = path matched, method did not)",
        );

        // A `/ports` discovery route existed earlier on this branch and was
        // withdrawn: the REST surface reports bindings on the box resource.
        let withdrawn_ports = http
            .get(format!("http://127.0.0.1:{port}/v1/boxes/box1/ports"))
            .send()
            .await
            .expect("GET withdrawn /ports route");
        assert_eq!(
            withdrawn_ports.status().as_u16(),
            404,
            "the withdrawn /ports discovery route must not come back",
        );

        let unrouted = http
            .get(format!("http://127.0.0.1:{port}/v1/boxes/box1/attach/nope"))
            .send()
            .await
            .expect("GET unregistered path");
        assert_eq!(
            unrouted.status().as_u16(),
            404,
            "control: an unregistered path must 404, so the 405 above is meaningful",
        );

        server.abort();
    }

    // ---------------------------------------------------------------
    // Finding 1: late subscriber misses pre-attach output
    // ---------------------------------------------------------------
    //
    // ActiveExecution pumps stdout through a tokio::sync::broadcast
    // sender. broadcast::subscribe() only delivers messages sent AFTER
    // the subscribe call. A client that calls GET /attach after output
    // has already been produced loses that output.
    //
    // This exercises the real ActiveExecution: we push lines through
    // the stub, let the pump broadcast them, then subscribe and check
    // whether the late subscriber sees the earlier lines.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_attach_subscriber_must_see_prior_output() {
        let (active, stdout_tx, _stderr_tx, _result_tx) = make_test_active();

        // Push 5 lines through the stub's stdout channel. The pump
        // task inside ActiveExecution::new reads these and broadcasts
        // them.
        for i in 1..=5 {
            stdout_tx.send(format!("line-{i}\n")).unwrap();
        }
        // Give the pump task a tick to broadcast all 5 chunks.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // NOW subscribe — this is what run_attach_session does when a
        // client connects to /attach after the exec already produced
        // output.
        let mut rx = active.stdout_bus().subscribe();

        // Push one more line AFTER the subscribe so we can prove the
        // channel is alive.
        stdout_tx.send("line-6\n".to_string()).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut received = Vec::new();
        while let Ok(Ok(data)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            received.push(String::from_utf8(data).unwrap());
        }

        // MUST FAIL on unfixed code: received has only ["line-6\n"].
        // The 5 pre-subscribe lines are lost because broadcast has no
        // backlog replay.
        assert!(
            received.len() >= 6,
            "late subscriber must see pre-subscribe output; \
             got {} line(s): {:?}  (expected >= 6, including the 5 pre-attach lines)",
            received.len(),
            received,
        );
    }

    // ---------------------------------------------------------------
    // Finding 2: final stdout chunk lost on fast process exit
    // ---------------------------------------------------------------
    //
    // The architecture has TWO independent spawned tasks:
    //   (A) stdout pump: reads ExecStdout stream → broadcasts via stdout_bus
    //   (B) wait task: calls execution.wait() → stores exit_code → fires done_tx
    //
    // If (B) fires done_tx BEFORE (A) has broadcast the last chunk,
    // the WS writer's try_recv() drain misses it.
    //
    // Rather than racing the scheduler, we test the structural defect
    // directly: done_tx can fire while the pump's broadcast channel
    // still has unconsumed source items in the ExecStdout mpsc.
    // A correct implementation would barrier the pump's completion
    // before firing done_tx.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn done_signal_must_wait_for_pump_completion() {
        let (active, stdout_tx, stderr_tx, result_tx) = make_test_active();

        // Subscribe BEFORE any data is pushed — the subscriber will
        // receive all broadcast chunks. No sleep-based polling.
        let mut rx = active.stdout_bus().subscribe();
        let mut done_rx = active.done_rx();

        // Push output, then signal exit immediately. The pump task
        // must read from ExecStdout and broadcast BEFORE done fires.
        stdout_tx.send("final-line\n".to_string()).unwrap();
        drop(stdout_tx);
        drop(stderr_tx);
        result_tx
            .send(boxlite::ExecResult {
                exit_code: 0,
                error_message: None,
            })
            .unwrap();

        // Wait for the done signal.
        let _ = tokio::time::timeout(Duration::from_secs(2), done_rx.changed()).await;

        // After done fires, the pump barrier guarantees all output has
        // been broadcast. Drain with try_recv — no sleep needed.
        let mut all = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            all.push(String::from_utf8(bytes).unwrap());
        }

        assert!(
            all.iter().any(|s| s.contains("final-line")),
            "after done_rx fires, all output must have been broadcast; \
             got: {:?}",
            all,
        );
    }

    // ---------------------------------------------------------------
    // Finding 3: reaper immediately evicts completed execs
    // ---------------------------------------------------------------
    //
    // run_reap_once removes is_done() execs on the very next tick.
    // The Go runner retains them for 5 minutes. A client that polls
    // GET /executions/{id} shortly after exit gets 404.
    //
    // We can't construct a full AppState without BoxliteRuntime, so
    // we build the executions map directly and call run_reap_once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaper_retains_completed_exec_for_grace_period() {
        let (active, stdout_tx, stderr_tx, result_tx) = make_test_active();

        // Signal exit so is_done() flips true. Drop BOTH stream senders
        // so the pump tasks exit and the wait task's barrier completes.
        drop(stdout_tx);
        drop(stderr_tx);
        result_tx
            .send(boxlite::ExecResult {
                exit_code: 42,
                error_message: None,
            })
            .unwrap();
        for _ in 0..20 {
            if active.is_done() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(active.is_done(), "precondition: exec must be done");

        // The exec was just created so created_at is recent — the
        // production should_retain() must return true.
        let now = Instant::now();
        assert!(
            active.should_retain(now),
            "recently-completed exec must be retained (grace period = {:?})",
            COMPLETED_RETENTION_GRACE,
        );

        // Conversely, a time far in the future should NOT retain.
        let far_future = now + COMPLETED_RETENTION_GRACE + Duration::from_secs(1);
        assert!(
            !active.should_retain(far_future),
            "exec past the retention grace must not be retained",
        );
    }

    /// A finished main session must not be handed to a restarted box.
    ///
    /// Unlike an exec — which gets a fresh id every time — the main session's id
    /// is the container id, fixed at box creation and therefore identical across
    /// reboots. So the previous run's dead session still matches this box, and
    /// without an `is_done()` filter a post-restart attach would be given the
    /// old VM's stream, its stale backlog and its stale exit code, while the new
    /// boot's init session was never registered at all.
    #[tokio::test]
    async fn find_main_session_skips_a_finished_one_so_a_restart_gets_a_new_session() {
        let (exec, channels) = stub_execution("cid-main");
        let finished = ActiveExecution::new("box1".to_string(), SessionKind::Main, exec, None);

        let mut executions = HashMap::new();
        executions.insert("cid-main".to_string(), Arc::clone(&finished));

        assert!(
            find_main_session(&executions, "box1").is_some(),
            "precondition: a live main session is found"
        );

        // End it, exactly as init exiting would: send the result and drop the
        // stream senders so the pumps finish.
        let StubChannels(stdout_tx, stderr_tx, _stdin_rx, result_tx) = channels;
        drop(stdout_tx);
        drop(stderr_tx);
        result_tx
            .send(boxlite::ExecResult {
                exit_code: 0,
                error_message: None,
            })
            .unwrap();
        for _ in 0..40 {
            if finished.is_done() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(finished.is_done(), "precondition: the main session ended");

        assert!(
            find_main_session(&executions, "box1").is_none(),
            "a finished main session must not be reused — the restarted box needs a new one"
        );
    }

    /// The reaper must never reap the box's main session.
    ///
    /// That session is the container's init, so killing it powers the VM off
    /// and takes the whole box with it. Registering it in `state.executions` is
    /// what makes `/boxes/{id}/attach` work — but that map is the one the
    /// reaper walks, which put the user's box on the orphan-escalation path
    /// (SIGHUP → SIGTERM → SIGKILL → evict) the moment their client
    /// disconnected, and under the 24h lifetime cap even while still attached.
    /// Detaching from `docker attach` does not stop a container; nor may this.
    #[tokio::test]
    async fn reaper_never_reaps_the_boxs_main_session() {
        let runtime = BoxliteRuntime::rest(boxlite::BoxliteRestOptions::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .expect("rest runtime");
        let state = AppState {
            runtime,
            boxes: RwLock::new(HashMap::new()),
            executions: RwLock::new(HashMap::new()),
            api_key: None,
            lifecycle: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
        };

        // The box's main command, and an ordinary exec running beside it.
        let (init, _init_channels) = stub_execution("cid-main");
        let main = Arc::new(ActiveExecution::new(
            "box1".to_string(),
            SessionKind::Main,
            init,
            None,
        ));
        let (tenant_exec, _tenant_channels) = stub_execution("exec-1");
        let tenant = Arc::new(ActiveExecution::new(
            "box1".to_string(),
            SessionKind::Exec,
            tenant_exec,
            None,
        ));
        {
            let mut map = state.executions.write().await;
            map.insert("cid-main".to_string(), Arc::clone(&main));
            map.insert("exec-1".to_string(), Arc::clone(&tenant));
        }

        // Neither was ever attached, and we reap from far enough in the future
        // that the lifetime cap has long since passed — the harshest state the
        // reaper knows.
        let doomsday = Instant::now() + Duration::from_secs(48 * 60 * 60);
        run_reap_once(
            &state,
            doomsday,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(60),
        )
        .await;

        let surviving = state.executions.read().await;
        assert!(
            surviving.contains_key("cid-main"),
            "the box's main session must survive the reaper — reaping it kills init and destroys the box"
        );
        assert!(
            !main.is_reaping_kill().await,
            "the main session must never even be marked for kill"
        );

        // Control: the exec beside it *is* reaped under the same tick, so this
        // proves the Main guard rather than a reaper that happens to be inert.
        assert!(
            !surviving.contains_key("exec-1"),
            "an orphaned exec past the lifetime cap must still be killed and evicted"
        );
    }
}
