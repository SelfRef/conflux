//! End-to-end tests for the dry-run planner (`conflux sync --dry-run`), using
//! the offline filesystem backend so no network/daemon is involved.
//!
//! The planner must (a) predict the same changes the real sync applies, and
//! (b) touch nothing while doing so.

use conflux_core::backend::{self, Backend};
use conflux_core::config::{Remote, Sync};
use conflux_core::engine::{self, PlanOp, SyncPlan, Winner};
use conflux_core::index::Index;
use conflux_core::model::{Deletions, EmptyDirMode, Scope};
use conflux_core::Config;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// A filesystem-backed sync: local tree `local`, "remote" mirror under
/// `<remote_root>/data`.
fn config(local: &Path, remote_root: &Path) -> Config {
    let toml = format!(
        r#"
        [[remote]]
        id = "fs"
        backend = "filesystem"
        url = "{remote}"

        [[sync]]
        remote = "fs"
        local = "{local}"
        remote_path = "data"
        trigger = "manual"
        "#,
        remote = remote_root.display(),
        local = local.display(),
    );
    Config::from_toml_str(&toml).expect("config should parse")
}

fn parts(cfg: &Config) -> (&Sync, &Remote) {
    let sync = &cfg.syncs[0];
    (sync, cfg.remote(sync.remote_id()).unwrap())
}

fn new_backend(cfg: &Config) -> Box<dyn Backend> {
    let (sync, remote) = parts(cfg);
    backend::build(remote, sync, Path::new("/unused-for-fs")).unwrap()
}

fn plan(cfg: &Config, index: &Index) -> SyncPlan {
    let (sync, _) = parts(cfg);
    engine::plan_group(
        sync,
        new_backend(cfg).as_ref(),
        index,
        EmptyDirMode::Ignore,
        Scope::Mirror,
        Deletions::Allow,
        0,
        &[],
    )
    .expect("plan should succeed")
}

fn sync(cfg: &Config, index: &mut Index) -> engine::SyncReport {
    let (sync, _) = parts(cfg);
    engine::sync_group(
        sync,
        new_backend(cfg).as_ref(),
        index,
        false,
        EmptyDirMode::Ignore,
        Scope::Mirror,
        Deletions::Allow,
        0,
        &[],
    )
    .expect("sync should succeed")
}

/// Index the plan's ops by path for easy assertions.
fn ops(plan: &SyncPlan) -> BTreeMap<String, PlanOp> {
    plan.changes
        .iter()
        .map(|c| (c.path.as_str().to_string(), c.op))
        .collect()
}

fn set_mtime(path: &Path, epoch_secs: u64) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(epoch_secs))
        .unwrap();
}

#[test]
fn plan_predicts_pushes_and_pulls_without_touching_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    let remote_data = remote.join("data");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&remote_data).unwrap();

    // Local-only and remote-only files, nothing synced yet (empty index).
    fs::write(local.join("push.txt"), b"local").unwrap();
    fs::write(remote_data.join("pull.txt"), b"remote").unwrap();

    let cfg = config(&local, &remote);
    let index = Index::default();
    let p = plan(&cfg, &index);
    let ops = ops(&p);

    assert_eq!(ops.len(), 2, "two files, one each direction");
    assert_eq!(ops["push.txt"], PlanOp::Push { update: false });
    assert_eq!(ops["pull.txt"], PlanOp::Pull { update: false });

    // The dry run must not have created either file on the other side, nor
    // written an index.
    assert!(
        !remote_data.join("push.txt").exists(),
        "push must not happen"
    );
    assert!(!local.join("pull.txt").exists(), "pull must not happen");
    assert!(index.entries.is_empty(), "index untouched");
}

#[test]
fn plan_matches_the_sync_that_follows_it() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    let remote_data = remote.join("data");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&remote_data).unwrap();

    let cfg = config(&local, &remote);
    let mut index = Index::default();

    // Establish a baseline: three files synced both ways.
    fs::write(local.join("keep.txt"), b"v1").unwrap();
    fs::write(local.join("push.txt"), b"gone soon").unwrap();
    fs::write(remote_data.join("only_remote.txt"), b"r").unwrap();
    sync(&cfg, &mut index);
    assert!(local.join("only_remote.txt").exists(), "baseline pulled");
    assert!(remote_data.join("keep.txt").exists(), "baseline pushed");

    // Now diverge: edit a file locally, delete another locally, add one on the
    // remote.
    fs::write(local.join("keep.txt"), b"v2 edited").unwrap();
    fs::remove_file(local.join("push.txt")).unwrap();
    fs::write(remote_data.join("fresh.txt"), b"new remote").unwrap();

    let predicted = ops(&plan(&cfg, &index));
    assert_eq!(predicted["keep.txt"], PlanOp::Push { update: true });
    assert_eq!(predicted["push.txt"], PlanOp::DeleteRemote);
    assert_eq!(predicted["fresh.txt"], PlanOp::Pull { update: false });

    // The remote still has the file the plan said it would delete, and the
    // local tree still lacks the file the plan said it would pull.
    assert!(remote_data.join("push.txt").exists(), "plan is read-only");
    assert!(!local.join("fresh.txt").exists(), "plan is read-only");

    // Applying the sync produces exactly the changes the plan predicted.
    let report = sync(&cfg, &mut index);
    assert_eq!(report.modified, vec![rel("keep.txt")]);
    assert_eq!(report.removed, vec![rel("push.txt")]);
    assert_eq!(report.added, vec![rel("fresh.txt")]);
}

#[test]
fn plan_predicts_the_conflict_winner_by_mtime() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    let remote_data = remote.join("data");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&remote_data).unwrap();

    let cfg = config(&local, &remote);
    let mut index = Index::default();

    // Baseline: one shared file.
    fs::write(local.join("doc.txt"), b"base").unwrap();
    sync(&cfg, &mut index);

    // Both sides edit it differently; make the local copy the newer one.
    fs::write(local.join("doc.txt"), b"local edit").unwrap();
    fs::write(remote_data.join("doc.txt"), b"remote edit").unwrap();
    set_mtime(&remote_data.join("doc.txt"), 1_000);
    set_mtime(&local.join("doc.txt"), 2_000);

    let predicted = ops(&plan(&cfg, &index));
    assert_eq!(
        predicted["doc.txt"],
        PlanOp::Conflict {
            winner: Winner::Local
        }
    );

    // Remote's losing content is still intact — the plan changed nothing.
    assert_eq!(
        fs::read_to_string(remote_data.join("doc.txt")).unwrap(),
        "remote edit"
    );
}

fn rel(s: &str) -> conflux_core::relpath::RelPath {
    conflux_core::relpath::RelPath::from_relative(Path::new(s)).unwrap()
}
