//! End-to-end engine tests using the local backend and real temp directories.

use conflux_core::backend;
use conflux_core::engine::{self, SyncReport, Winner};
use conflux_core::index::Index;
use conflux_core::Config;
use filetime::FileTime;
use std::fs;
use std::path::{Path, PathBuf};

/// Build a one-group config: local `root` mirrored to `<remote>/data`.
fn config(local: &Path, remote: &Path, extra: &str) -> Config {
    let toml = format!(
        r#"
        [[remote]]
        name = "m"
        type = "local"
        url = "{remote}"

        [[sync]]
        remote = "m"
        root = "{local}"
        remote_path = "data"
        trigger = "manual"
        {extra}
        "#,
        remote = remote.display(),
        local = local.display(),
    );
    Config::from_toml_str(&toml).expect("config should parse")
}

fn run(cfg: &Config, index: &mut Index) -> SyncReport {
    let sync = &cfg.syncs[0];
    let remote = cfg.remote(&sync.remote).unwrap();
    let backend = backend::build(remote, sync, Path::new("/unused-for-local")).unwrap();
    engine::sync_group(sync, backend.as_ref(), index).expect("sync should succeed")
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn set_mtime(path: &Path, secs: i64) {
    filetime::set_file_mtime(path, FileTime::from_unix_time(secs, 0)).unwrap();
}

/// Find a `*.conflux-conflict-*` file in `dir`, if any.
fn find_conflict_copy(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.contains(".conflux-conflict-") {
            Some(e.path())
        } else {
            None
        }
    })
}

struct Dirs {
    _tmp: tempfile::TempDir,
    local: PathBuf,
    remote: PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&remote).unwrap();
    Dirs {
        _tmp: tmp,
        local,
        remote,
    }
}

#[test]
fn pushes_new_local_file_then_is_idempotent() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    write(&d.local.join("a.txt"), "hello");
    let report = run(&cfg, &mut index);
    assert_eq!(report.pushed.len(), 1);
    assert_eq!(read(&d.remote.join("data/a.txt")), "hello");

    // Second run: nothing to do.
    let report = run(&cfg, &mut index);
    assert!(
        report.is_empty(),
        "second run should be a no-op: {report:?}"
    );
}

#[test]
fn pulls_new_remote_file() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    write(&d.remote.join("data/b.txt"), "remote-side");
    let report = run(&cfg, &mut index);
    assert_eq!(report.pulled.len(), 1);
    assert_eq!(read(&d.local.join("b.txt")), "remote-side");
}

#[test]
fn propagates_deletes_both_ways() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    // Establish a baseline with two files.
    write(&d.local.join("keep.txt"), "1");
    write(&d.remote.join("data/fromremote.txt"), "2");
    run(&cfg, &mut index);
    assert!(d.local.join("fromremote.txt").exists());
    assert!(d.remote.join("data/keep.txt").exists());

    // Delete one on each side; deletes should propagate.
    fs::remove_file(d.local.join("keep.txt")).unwrap();
    fs::remove_file(d.remote.join("data/fromremote.txt")).unwrap();
    let report = run(&cfg, &mut index);
    assert_eq!(report.deleted_remote.len(), 1);
    assert_eq!(report.deleted_local.len(), 1);
    assert!(!d.remote.join("data/keep.txt").exists());
    assert!(!d.local.join("fromremote.txt").exists());
}

#[test]
fn conflict_newer_local_wins_and_preserves_remote_copy() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    // Baseline.
    write(&d.local.join("c.txt"), "base");
    run(&cfg, &mut index);

    // Diverge both sides with different content; make local strictly newer.
    write(&d.local.join("c.txt"), "LOCAL");
    write(&d.remote.join("data/c.txt"), "REMOTE");
    set_mtime(&d.remote.join("data/c.txt"), 1_000);
    set_mtime(&d.local.join("c.txt"), 2_000);

    let report = run(&cfg, &mut index);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].winner, Winner::Local);

    // Remote now holds the local (winning) content.
    assert_eq!(read(&d.remote.join("data/c.txt")), "LOCAL");
    // The losing remote content is preserved as a local conflict copy.
    let copy = find_conflict_copy(&d.local).expect("conflict copy should exist");
    assert_eq!(read(&copy), "REMOTE");
}

#[test]
fn pull_only_applies_remote_changes_but_never_pushes() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"direction = "pull""#);
    let mut index = Index::default();

    // A local-only file must never be pushed in pull-only mode.
    write(&d.local.join("local-only.txt"), "mine");
    // A remote file must be pulled.
    write(&d.remote.join("data/down.txt"), "incoming");
    let report = run(&cfg, &mut index);

    assert!(report.pushed.is_empty(), "pull-only must not push");
    assert_eq!(report.pulled.len(), 1);
    assert_eq!(read(&d.local.join("down.txt")), "incoming");
    assert!(
        !d.remote.join("data/local-only.txt").exists(),
        "local-only file must not reach the remote"
    );
}

#[test]
fn include_restricts_what_is_pushed() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"include = ["keep"]"#);
    let mut index = Index::default();

    write(&d.local.join("keep/a.txt"), "yes");
    write(&d.local.join("other/b.txt"), "no");
    let report = run(&cfg, &mut index);

    assert_eq!(report.pushed.len(), 1);
    assert!(d.remote.join("data/keep/a.txt").exists());
    assert!(
        !d.remote.join("data/other/b.txt").exists(),
        "non-included paths must not be pushed"
    );
}
