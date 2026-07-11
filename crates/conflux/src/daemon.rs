//! The `conflux daemon` runtime: a single-worker scheduler driven by manual,
//! timer, and file-watch triggers, plus a Unix-socket control server.
//!
//! Only this module is async. Each sync runs on a blocking thread
//! (`spawn_blocking`) and is serialized daemon-wide by `sync_lock`, so the
//! synchronous engine never has two runs touching one group's index at once.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use conflux_core::config::{Remote, Sync};
use conflux_core::engine::{self, group_label, SyncSummary};
use conflux_core::ipc::{GroupOutcome, GroupStatus, Request, Response, SyncTarget};
use conflux_core::model::{Deletions, EmptyDirMode, RemoteKind};
use conflux_core::{Config, Paths};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// Runtime status recorded per group (merged with static config for IPC).
#[derive(Default, Clone)]
struct RuntimeStatus {
    last_run: Option<u64>,
    last_summary: Option<SyncSummary>,
    last_error: Option<String>,
}

/// Mutable daemon state behind a short-lived lock (never held across `.await`).
struct DaemonState {
    config: Config,
    paths: Paths,
    status: BTreeMap<String, RuntimeStatus>,
}

type Shared = Arc<Mutex<DaemonState>>;

/// Submits sync jobs to the worker, coalescing duplicates already queued.
///
/// `queued` maps a label to whether its pending job is pull-only. A full sync
/// supersedes a queued pull-only job for the same label, so background pulls
/// never mask a real bidirectional run.
#[derive(Clone)]
struct Submitter {
    tx: mpsc::UnboundedSender<String>,
    queued: Arc<Mutex<HashMap<String, bool>>>,
}

impl Submitter {
    /// Queue a full sync (applies both local and remote changes, per the group's scope).
    fn submit(&self, label: String) {
        self.enqueue(label, false);
    }

    /// Queue a pull-only run (used by the periodic `pull_interval` trigger).
    fn submit_pull(&self, label: String) {
        self.enqueue(label, true);
    }

    fn enqueue(&self, label: String, pull_only: bool) {
        let mut queued = self.queued.lock().unwrap();
        match queued.get_mut(&label) {
            // Already queued: upgrade a pull-only job to a full sync if needed.
            Some(existing) => {
                if !pull_only {
                    *existing = false;
                }
            }
            None => {
                queued.insert(label.clone(), pull_only);
                let _ = self.tx.send(label);
            }
        }
    }
}

/// Handles for the active trigger tasks; aborted and rebuilt on reload.
#[derive(Default)]
struct Triggers {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    // File watchers must be kept alive; dropping them stops watching.
    watchers: Vec<Box<dyn Any + Send>>,
}

impl Triggers {
    fn shutdown(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.watchers.clear();
    }
}

/// Entry point: build the runtime, install triggers, and run until a signal.
pub fn run(config: Config, paths: Paths) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(run_async(config, paths))
}

async fn run_async(config: Config, paths: Paths) -> anyhow::Result<()> {
    let socket_path = paths.socket.clone();
    let state: Shared = Arc::new(Mutex::new(DaemonState {
        config,
        paths,
        status: BTreeMap::new(),
    }));
    let sync_lock = Arc::new(tokio::sync::Mutex::new(()));

    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let submit = Submitter {
        tx,
        queued: Arc::new(Mutex::new(HashMap::new())),
    };

    // Single worker: drains the job queue, one sync at a time.
    tokio::spawn(worker_loop(
        state.clone(),
        sync_lock.clone(),
        rx,
        submit.queued.clone(),
    ));

    let mut triggers = spawn_triggers(&state, &submit);

    // Sync every active group once at startup, warning if there is nothing to do.
    {
        let st = state.lock().unwrap();
        for lint in st.config.warnings() {
            warn!("{lint}");
        }
        let labels = active_labels(&st);
        if labels.is_empty() {
            if st.config.syncs.is_empty() {
                warn!("no sync groups configured — add a [[remote]] and [[sync]] to the config; the daemon has nothing to do");
            } else {
                let profile = st.config.daemon.profile.as_deref().unwrap_or("default");
                warn!(profile, "no sync groups active for this profile — check each sync's `profiles`; the daemon has nothing to do");
            }
        }
        drop(st);
        for label in labels {
            submit.submit(label);
        }
    }

    let listener = bind_socket(&socket_path)?;
    let (reload_tx, mut reload_rx) = mpsc::unbounded_channel::<oneshot::Sender<(bool, String)>>();
    tokio::spawn(serve_ipc(
        state.clone(),
        sync_lock.clone(),
        submit.clone(),
        listener,
        reload_tx.clone(),
    ));

    info!(socket = %socket_path.display(), "conflux daemon started");

    let mut sighup = signal(SignalKind::hangup()).context("install SIGHUP handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => { info!("received SIGTERM, shutting down"); break; }
            _ = sigint.recv()  => { info!("received SIGINT, shutting down");  break; }
            _ = sighup.recv()  => {
                let (ok, msg) = reload(&state);
                if ok {
                    triggers.shutdown();
                    triggers = spawn_triggers(&state, &submit);
                }
                info!(ok, "reload via SIGHUP: {msg}");
            }
            Some(reply) = reload_rx.recv() => {
                let (ok, msg) = reload(&state);
                if ok {
                    triggers.shutdown();
                    triggers = spawn_triggers(&state, &submit);
                }
                let _ = reply.send((ok, msg));
            }
        }
    }

    triggers.shutdown();
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Backoff delays between retry attempts for triggered syncs.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

/// The worker: run queued syncs sequentially, retrying transient failures.
async fn worker_loop(
    state: Shared,
    sync_lock: Arc<tokio::sync::Mutex<()>>,
    mut rx: mpsc::UnboundedReceiver<String>,
    queued: Arc<Mutex<HashMap<String, bool>>>,
) {
    while let Some(label) = rx.recv().await {
        // A job queued only via `pull_interval` runs as a remote refresh.
        let remote_refresh = queued.lock().unwrap().remove(&label).unwrap_or(false);

        // One attempt plus RETRY_BACKOFF.len() retries.
        for attempt in 0..=RETRY_BACKOFF.len() {
            match run_one(&state, &sync_lock, &label, remote_refresh).await {
                Some(outcome) if outcome.error.is_some() => {
                    let err = outcome.error.unwrap();
                    if let Some(delay) = RETRY_BACKOFF.get(attempt) {
                        warn!(group = %label, "sync failed, retrying in {delay:?}: {err}");
                        tokio::time::sleep(*delay).await;
                    } else {
                        error!(group = %label, "sync failed after retries: {err}");
                    }
                }
                // Success or the group no longer exists — stop retrying.
                _ => break,
            }
        }
    }
}

/// Resolve, run, and record one group's sync. Serialized by `sync_lock`.
///
/// `remote_refresh` marks a periodic `pull_interval` run: it only applies
/// remote-side changes to a bidirectional group, so it never clobbers a pending
/// local edit/delete that the group's own trigger will reconcile.
async fn run_one(
    state: &Shared,
    sync_lock: &Arc<tokio::sync::Mutex<()>>,
    label: &str,
    remote_refresh: bool,
) -> Option<GroupOutcome> {
    let (sync, remote, state_dir, empty_dirs, deletions, max_file_size, exclude_defaults) = {
        let st = state.lock().unwrap();
        let (sync, remote, state_dir) = resolve(&st, label)?;
        let empty_dirs = resolve_empty_dirs(&st.config, &sync);
        let deletions = resolve_deletions(&st.config, &sync);
        let max_file_size = resolve_max_file_size(&st.config, &sync);
        let exclude_defaults = st.config.daemon.exclude.clone();
        (
            sync,
            remote,
            state_dir,
            empty_dirs,
            deletions,
            max_file_size,
            exclude_defaults,
        )
    };

    let _guard = sync_lock.lock().await;
    // A periodic remote-refresh run only brings changes in — call that "pulled"
    // rather than "synced".
    let verb = if remote_refresh { "pulled" } else { "synced" };
    debug!(group = %label, "running {verb}");
    let join = tokio::task::spawn_blocking(move || {
        engine::run(
            &sync,
            &remote,
            &state_dir,
            remote_refresh,
            empty_dirs,
            deletions,
            max_file_size,
            &exclude_defaults,
        )
    })
    .await;

    let (summary, error) = match join {
        Ok(Ok(report)) => {
            for c in &report.conflicts {
                warn!(group = %label, path = %c.path, "conflict resolved ({:?}); copy at {}", c.winner, c.conflict_copy.display());
            }
            if !report.is_empty() {
                info!(group = %label, "{verb}: {}", SyncSummary::from(&report));
            } else {
                debug!(group = %label, "{verb} complete, no changes");
            }
            (Some(SyncSummary::from(&report)), None)
        }
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(e) => (None, Some(format!("sync task failed: {e}"))),
    };

    {
        let mut st = state.lock().unwrap();
        let entry = st.status.entry(label.to_string()).or_default();
        entry.last_run = Some(now_secs());
        entry.last_summary = summary.clone();
        entry.last_error = error.clone();
    }

    Some(GroupOutcome {
        label: label.to_string(),
        summary,
        error,
    })
}

/// Spawn timer and watch trigger tasks for every active group.
fn spawn_triggers(state: &Shared, submit: &Submitter) -> Triggers {
    let mut triggers = Triggers::default();
    let st = state.lock().unwrap();
    let active_profile = st.config.daemon.profile.as_deref();

    for sync in &st.config.syncs {
        if !is_active(sync, active_profile) {
            continue;
        }
        let label = group_label(sync);

        // Git can't store empty directories, so `empty_dirs` has no effect there.
        if resolve_empty_dirs(&st.config, sync) != EmptyDirMode::Ignore
            && st.config.remote(sync.remote_id()).map(|r| r.backend) == Some(RemoteKind::Git)
        {
            warn!(
                group = %label,
                "`empty_dirs` is set but the remote is a git repo, which cannot \
                 store empty directories; the setting is ignored for this group"
            );
        }

        // Set when `watch-both` installs a live watcher on the remote side, so
        // the periodic `pull_interval` poll below can be skipped as redundant.
        let mut remote_watched = false;

        match sync.trigger {
            conflux_core::model::Trigger::Manual => {}
            conflux_core::model::Trigger::Timer => {
                let interval = sync.effective_interval(st.config.daemon.interval);
                let submit = submit.clone();
                let label = label.clone();
                triggers.tasks.push(tokio::spawn(async move {
                    let mut tick = tokio::time::interval(interval);
                    tick.tick().await; // consume the immediate first tick
                    loop {
                        tick.tick().await;
                        submit.submit(label.clone());
                    }
                }));
            }
            conflux_core::model::Trigger::Watch => {
                let debounce = resolve_debounce(&st.config, sync);
                install_watch(
                    &mut triggers,
                    &sync.local,
                    debounce,
                    submit,
                    &label,
                    "local",
                );
            }
            conflux_core::model::Trigger::WatchBoth => {
                let debounce = resolve_debounce(&st.config, sync);
                install_watch(
                    &mut triggers,
                    &sync.local,
                    debounce,
                    submit,
                    &label,
                    "local",
                );
                // Also watch the remote's filesystem path. Config validation
                // guarantees a `filesystem` backend here, so this always applies.
                if let Some(r) = st.config.remote(sync.remote_id()) {
                    if r.backend == RemoteKind::Filesystem {
                        let remote_dir = conflux_core::backend::local::base_path(r, sync);
                        install_watch(
                            &mut triggers,
                            &remote_dir,
                            debounce,
                            submit,
                            &label,
                            "remote",
                        );
                        remote_watched = true;
                    }
                }
            }
        }

        // Independent of `trigger`: periodically pull the remote to pick up
        // remote-side changes. Disabled when unset or zero, and skipped when the
        // remote is already watched live (`watch-both` on a filesystem backend).
        if remote_watched {
            if resolve_pull_interval(&st.config, sync).is_some_and(|d| !d.is_zero()) {
                debug!(group = %label, "ignoring pull_interval: the remote is already watched via watch-both");
            }
        } else if let Some(pull_interval) = resolve_pull_interval(&st.config, sync) {
            if !pull_interval.is_zero() {
                debug!(group = %label, ?pull_interval, "scheduling periodic remote pull");
                let submit = submit.clone();
                let label = label.clone();
                triggers.tasks.push(tokio::spawn(async move {
                    let mut tick = tokio::time::interval(pull_interval);
                    tick.tick().await; // consume the immediate first tick
                    loop {
                        tick.tick().await;
                        debug!(group = %label, "pull interval elapsed, queuing remote refresh");
                        submit.submit_pull(label.clone());
                    }
                }));
            }
        }
    }
    triggers
}

/// Install a recursive watcher on `path`, pushing it into `triggers` on success
/// or logging a clear warning on failure. `role` (`"local"`/`"remote"`) is used
/// only in the messages.
fn install_watch(
    triggers: &mut Triggers,
    path: &Path,
    debounce: Duration,
    submit: &Submitter,
    label: &str,
    role: &str,
) {
    match spawn_watch(path, debounce, submit.clone(), label.to_string()) {
        Ok(watcher) => triggers.watchers.push(watcher),
        Err(e) => {
            let disp = path.display();
            if !path.exists() {
                warn!(
                    group = %label,
                    "not watching {role} directory `{disp}`: it does not exist. \
                     Create it (or fix the config) and reload; this group will not \
                     sync from that side until then",
                );
            } else {
                warn!(group = %label, "not watching {role} directory `{disp}`: {e:#}");
            }
        }
    }
}

/// Set up a debounced recursive watcher that submits the group on change.
fn spawn_watch(
    root: &Path,
    debounce: Duration,
    submit: Submitter,
    label: String,
) -> anyhow::Result<Box<dyn Any + Send>> {
    let mut debouncer = new_debouncer(debounce, None, move |res: DebounceEventResult| {
        if let Ok(events) = res {
            if !events.is_empty() {
                submit.submit(label.clone());
            }
        }
    })
    .context("create file watcher")?;
    debouncer
        .watcher()
        .watch(root, RecursiveMode::Recursive)
        .context("register recursive watch")?;
    debouncer.cache().add_root(root, RecursiveMode::Recursive);
    Ok(Box::new(debouncer))
}

/// Accept IPC connections and dispatch each request.
async fn serve_ipc(
    state: Shared,
    sync_lock: Arc<tokio::sync::Mutex<()>>,
    submit: Submitter,
    listener: UnixListener,
    reload_tx: mpsc::UnboundedSender<oneshot::Sender<(bool, String)>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                let sync_lock = sync_lock.clone();
                let submit = submit.clone();
                let reload_tx = reload_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_conn(stream, &state, &sync_lock, &submit, &reload_tx).await
                    {
                        warn!("ipc connection error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("ipc accept failed: {e}");
            }
        }
    }
}

async fn handle_conn(
    stream: UnixStream,
    state: &Shared,
    sync_lock: &Arc<tokio::sync::Mutex<()>>,
    submit: &Submitter,
    reload_tx: &mpsc::UnboundedSender<oneshot::Sender<(bool, String)>>,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }

    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(Request::Status) => Response::Status(status_list(&state.lock().unwrap())),
        Ok(Request::Sync(target)) => {
            let labels = {
                let st = state.lock().unwrap();
                labels_for_target(&st, &target)
            };
            if labels.is_empty() {
                Response::Error("no matching sync groups".into())
            } else {
                let mut outcomes = Vec::new();
                for label in labels {
                    // Manual `conflux sync` always runs a full bidirectional sync.
                    if let Some(o) = run_one(state, sync_lock, &label, false).await {
                        outcomes.push(o);
                    }
                }
                let _ = submit; // manual syncs run inline; queue unused here
                Response::Synced(outcomes)
            }
        }
        Ok(Request::Reload) => {
            let (tx, rx) = oneshot::channel();
            if reload_tx.send(tx).is_err() {
                Response::Error("daemon is shutting down".into())
            } else {
                match rx.await {
                    Ok((ok, message)) => Response::Reloaded { ok, message },
                    Err(_) => Response::Error("reload was dropped".into()),
                }
            }
        }
        Err(e) => Response::Error(format!("bad request: {e}")),
    };

    let mut stream = reader.into_inner();
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Reload the config from disk, replacing it on success.
fn reload(state: &Shared) -> (bool, String) {
    let config_path = { state.lock().unwrap().paths.config.clone() };
    match Config::load(&config_path) {
        Ok(config) => {
            for lint in config.warnings() {
                warn!("{lint}");
            }
            let mut st = state.lock().unwrap();
            let groups = config.syncs.len();
            st.config = config;
            (true, format!("reloaded {groups} sync group(s)"))
        }
        Err(e) => (false, format!("reload failed, keeping current config: {e}")),
    }
}

fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket dir {}", parent.display()))?;
    }
    // Remove a stale socket from a previous run.
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path).with_context(|| format!("bind socket {}", path.display()))
}

fn resolve_debounce(config: &Config, sync: &Sync) -> Duration {
    sync.debounce.unwrap_or(config.daemon.debounce)
}

/// Resolve the periodic pull interval for a group: per-sync, then per-remote,
/// then the daemon default. `None` (or a resolved zero) disables background
/// pulling. The most specific level set wins, so a sync/remote can set `0` to
/// opt out even when a broader level enables it.
fn resolve_pull_interval(config: &Config, sync: &Sync) -> Option<Duration> {
    sync.pull_interval
        .or_else(|| {
            config
                .remote(sync.remote_id())
                .and_then(|r| r.pull_interval)
        })
        .or(config.daemon.pull_interval)
}

/// Resolve empty-directory handling for a group: per-sync, then per-remote, then
/// the daemon default.
fn resolve_empty_dirs(config: &Config, sync: &Sync) -> EmptyDirMode {
    sync.empty_dirs.unwrap_or(config.daemon.empty_dirs)
}

fn resolve_deletions(config: &Config, sync: &Sync) -> Deletions {
    sync.deletions.unwrap_or(config.daemon.deletions)
}

/// Resolve the max synced file size (bytes; `0` = unlimited) for a group:
/// per-sync, then per-remote, then the daemon default.
fn resolve_max_file_size(config: &Config, sync: &Sync) -> u64 {
    sync.max_file_size
        .or_else(|| {
            config
                .remote(sync.remote_id())
                .and_then(|r| r.max_file_size)
        })
        .unwrap_or(config.daemon.max_file_size)
}

fn is_active(sync: &Sync, active_profile: Option<&str>) -> bool {
    match active_profile {
        None => true,
        Some(profile) => sync.profiles.iter().any(|p| p == profile),
    }
}

fn active_labels(st: &DaemonState) -> Vec<String> {
    let active_profile = st.config.daemon.profile.as_deref();
    st.config
        .syncs
        .iter()
        .filter(|s| is_active(s, active_profile))
        .map(group_label)
        .collect()
}

fn resolve(st: &DaemonState, label: &str) -> Option<(Sync, Remote, PathBuf)> {
    let sync = st
        .config
        .syncs
        .iter()
        .find(|s| group_label(s) == label)?
        .clone();
    let remote = st.config.remote(sync.remote_id())?.clone();
    Some((sync, remote, st.paths.state.clone()))
}

/// Resolve a sync request to labels, always scoped to this daemon's active
/// profile: `All` covers every group the daemon runs, `Group` picks the one
/// whose `id` or `remote:remote_path` label matches.
fn labels_for_target(st: &DaemonState, target: &SyncTarget) -> Vec<String> {
    let active_profile = st.config.daemon.profile.as_deref();
    st.config
        .syncs
        .iter()
        .filter(|s| is_active(s, active_profile))
        .filter(|s| match target {
            SyncTarget::All => true,
            SyncTarget::Group(name) => {
                s.id.as_deref() == Some(name.as_str()) || &group_label(s) == name
            }
        })
        .map(group_label)
        .collect()
}

fn status_list(st: &DaemonState) -> Vec<GroupStatus> {
    let active_profile = st.config.daemon.profile.as_deref();
    st.config
        .syncs
        .iter()
        .filter(|s| is_active(s, active_profile))
        .map(|s| {
            let label = group_label(s);
            let rt = st.status.get(&label).cloned().unwrap_or_default();
            GroupStatus {
                label,
                remote: s.remote_id().to_string(),
                root: s.local.display().to_string(),
                trigger: format!("{:?}", s.trigger).to_lowercase(),
                last_run: rt.last_run,
                last_summary: rt.last_summary,
                last_error: rt.last_error,
            }
        })
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_named(config: &Config, remote_path: &str) -> Sync {
        config
            .syncs
            .iter()
            .find(|s| s.remote_path == remote_path)
            .expect("sync present")
            .clone()
    }

    #[test]
    fn pull_interval_resolves_sync_then_remote_then_daemon() {
        let config = Config::from_toml_str(
            r#"
            [daemon]
            pull_interval = "1h"

            [[remote]]
            id = "a"
            backend = "filesystem"
            url = "/tmp/a"

            [[remote]]
            id = "b"
            backend = "filesystem"
            url = "/tmp/b"
            pull_interval = "10m"

            [[sync]]
            remote = "a"
            local = "/tmp/la"
            remote_path = "inherit-daemon"
            trigger = "manual"

            [[sync]]
            remote = "b"
            local = "/tmp/lb"
            remote_path = "inherit-remote"
            trigger = "manual"

            [[sync]]
            remote = "b"
            local = "/tmp/lc"
            remote_path = "own"
            trigger = "manual"
            pull_interval = "5m"

            [[sync]]
            remote = "a"
            local = "/tmp/ld"
            remote_path = "opt-out"
            trigger = "manual"
            pull_interval = "0s"
        "#,
        )
        .unwrap();

        // No override anywhere but the daemon default.
        assert_eq!(
            resolve_pull_interval(&config, &sync_named(&config, "inherit-daemon")),
            Some(Duration::from_secs(3600))
        );
        // Remote-level default beats the daemon default.
        assert_eq!(
            resolve_pull_interval(&config, &sync_named(&config, "inherit-remote")),
            Some(Duration::from_secs(600))
        );
        // Per-sync value wins over everything.
        assert_eq!(
            resolve_pull_interval(&config, &sync_named(&config, "own")),
            Some(Duration::from_secs(300))
        );
        // A per-sync zero opts out even though the daemon default is set; the
        // spawn loop treats a resolved zero as "disabled".
        assert_eq!(
            resolve_pull_interval(&config, &sync_named(&config, "opt-out")),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn sync_target_matches_by_id_or_label() {
        let config = Config::from_toml_str(
            r#"
            [[remote]]
            id = "r"
            backend = "filesystem"
            url = "/tmp/r"

            [[sync]]
            id = "docs"
            remote = "r"
            local = "/tmp/a"
            remote_path = "documents"
            trigger = "manual"

            [[sync]]
            remote = "r"
            local = "/tmp/b"
            remote_path = "config"
            trigger = "manual"
        "#,
        )
        .unwrap();
        let st = DaemonState {
            config,
            paths: Paths {
                config: "/x".into(),
                state: "/x".into(),
                socket: "/x".into(),
            },
            status: BTreeMap::new(),
        };

        let target = |s: &str| SyncTarget::Group(s.to_string());
        // A group's `id` resolves to its canonical label.
        assert_eq!(labels_for_target(&st, &target("docs")), vec!["r:documents"]);
        // The `remote:remote_path` label works too, for groups with or without an id.
        assert_eq!(
            labels_for_target(&st, &target("r:documents")),
            vec!["r:documents"]
        );
        assert_eq!(
            labels_for_target(&st, &target("r:config")),
            vec!["r:config"]
        );
        // An unknown name matches nothing; `All` matches every active group.
        assert!(labels_for_target(&st, &target("nope")).is_empty());
        assert_eq!(labels_for_target(&st, &SyncTarget::All).len(), 2);
    }

    #[test]
    fn pull_interval_disabled_when_unset_anywhere() {
        let config = Config::from_toml_str(
            r#"
            [[remote]]
            id = "a"
            backend = "filesystem"
            url = "/tmp/a"

            [[sync]]
            remote = "a"
            local = "/tmp/la"
            remote_path = "x"
            trigger = "watch"
        "#,
        )
        .unwrap();
        assert_eq!(
            resolve_pull_interval(&config, &sync_named(&config, "x")),
            None
        );
    }
}
