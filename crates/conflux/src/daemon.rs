//! The `conflux daemon` runtime: a single-worker scheduler driven by manual,
//! timer, and file-watch triggers, plus a Unix-socket control server.
//!
//! Only this module is async. Each sync runs on a blocking thread
//! (`spawn_blocking`) and is serialized daemon-wide by `sync_lock`, so the
//! synchronous engine never has two runs touching one group's index at once.

use std::any::Any;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use conflux_core::config::{Remote, Sync};
use conflux_core::engine::{self, group_label, SyncSummary};
use conflux_core::ipc::{GroupOutcome, GroupStatus, Request, Response, SyncTarget};
use conflux_core::{Config, Paths};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

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
#[derive(Clone)]
struct Submitter {
    tx: mpsc::UnboundedSender<String>,
    queued: Arc<Mutex<HashSet<String>>>,
}

impl Submitter {
    fn submit(&self, label: String) {
        let mut queued = self.queued.lock().unwrap();
        if queued.insert(label.clone()) {
            let _ = self.tx.send(label);
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
        queued: Arc::new(Mutex::new(HashSet::new())),
    };

    // Single worker: drains the job queue, one sync at a time.
    tokio::spawn(worker_loop(
        state.clone(),
        sync_lock.clone(),
        rx,
        submit.queued.clone(),
    ));

    let mut triggers = spawn_triggers(&state, &submit);

    // Sync every active group once at startup.
    for label in active_labels(&state.lock().unwrap()) {
        submit.submit(label);
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
    queued: Arc<Mutex<HashSet<String>>>,
) {
    while let Some(label) = rx.recv().await {
        queued.lock().unwrap().remove(&label);

        // One attempt plus RETRY_BACKOFF.len() retries.
        for attempt in 0..=RETRY_BACKOFF.len() {
            match run_one(&state, &sync_lock, &label).await {
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
async fn run_one(
    state: &Shared,
    sync_lock: &Arc<tokio::sync::Mutex<()>>,
    label: &str,
) -> Option<GroupOutcome> {
    let (sync, remote, state_dir) = {
        let st = state.lock().unwrap();
        resolve(&st, label)?
    };

    let _guard = sync_lock.lock().await;
    let join = tokio::task::spawn_blocking(move || engine::run(&sync, &remote, &state_dir)).await;

    let (summary, error) = match join {
        Ok(Ok(report)) => {
            for c in &report.conflicts {
                warn!(group = %label, path = %c.path, "conflict resolved ({:?}); copy at {}", c.winner, c.conflict_copy.display());
            }
            if !report.is_empty() {
                let s = SyncSummary::from(&report);
                info!(group = %label, "synced: +{} -{} push, +{} -{} remote, {} conflicts",
                    s.pushed, s.deleted_local, s.pulled, s.deleted_remote, s.conflicts);
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
    let active_profile = st.config.daemon.active_profile.as_deref();

    for sync in &st.config.syncs {
        if !is_active(sync, active_profile) {
            continue;
        }
        let label = group_label(sync);
        match sync.trigger {
            conflux_core::model::Trigger::Manual => {}
            conflux_core::model::Trigger::Timer => {
                let interval = sync.effective_interval();
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
                match spawn_watch(&sync.root, debounce, submit.clone(), label.clone()) {
                    Ok(watcher) => triggers.watchers.push(watcher),
                    Err(e) => warn!(group = %label, "failed to watch {}: {e}", sync.root.display()),
                }
            }
        }
    }
    triggers
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
        .with_context(|| format!("watch {}", root.display()))?;
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
                    if let Some(o) = run_one(state, sync_lock, &label).await {
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
    sync.debounce
        .or_else(|| config.remote(&sync.remote).and_then(|r| r.debounce))
        .unwrap_or(config.daemon.debounce)
}

fn is_active(sync: &Sync, active_profile: Option<&str>) -> bool {
    match active_profile {
        None | Some("default") => true,
        Some(profile) => sync.profiles.iter().any(|p| p == profile),
    }
}

fn active_labels(st: &DaemonState) -> Vec<String> {
    let active_profile = st.config.daemon.active_profile.as_deref();
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
    let remote = st.config.remote(&sync.remote)?.clone();
    Some((sync, remote, st.paths.state.clone()))
}

fn labels_for_target(st: &DaemonState, target: &SyncTarget) -> Vec<String> {
    match target {
        SyncTarget::All => st.config.syncs.iter().map(group_label).collect(),
        SyncTarget::Group(label) => st
            .config
            .syncs
            .iter()
            .map(group_label)
            .filter(|l| l == label)
            .collect(),
        SyncTarget::Profile(profile) => st
            .config
            .syncs
            .iter()
            .filter(|s| s.profiles.iter().any(|p| p == profile))
            .map(group_label)
            .collect(),
    }
}

fn status_list(st: &DaemonState) -> Vec<GroupStatus> {
    let active_profile = st.config.daemon.active_profile.as_deref();
    st.config
        .syncs
        .iter()
        .filter(|s| is_active(s, active_profile))
        .map(|s| {
            let label = group_label(s);
            let rt = st.status.get(&label).cloned().unwrap_or_default();
            GroupStatus {
                label,
                remote: s.remote.clone(),
                root: s.root.display().to_string(),
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
