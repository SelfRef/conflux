//! End-to-end engine tests using the local backend and real temp directories.

use conflux_core::backend;
use conflux_core::engine::{self, SyncReport, Winner};
use conflux_core::index::Index;
use conflux_core::model::EmptyDirMode;
use conflux_core::Config;
use filetime::FileTime;
use std::fs;
use std::path::{Path, PathBuf};

/// Build a one-group config: local `root` mirrored to `<remote>/data`.
///
/// Most tests exercise whole-tree behavior, so unless `extra` sets its own
/// `scope` or `include`, the group defaults to `scope = "mirror"` (the shipped
/// default `scope = "include"` with an empty `include` would sync nothing).
fn config(local: &Path, remote: &Path, extra: &str) -> Config {
    let scope_line = if extra.contains("scope") || extra.contains("include") {
        ""
    } else {
        r#"scope = "mirror""#
    };
    let toml = format!(
        r#"
        [[remote]]
        id = "m"
        backend = "local"
        url = "{remote}"

        [[sync]]
        remote = "m"
        local = "{local}"
        remote_path = "data"
        trigger = "manual"
        {scope_line}
        {extra}
        "#,
        remote = remote.display(),
        local = local.display(),
    );
    Config::from_toml_str(&toml).expect("config should parse")
}

fn run(cfg: &Config, index: &mut Index) -> SyncReport {
    run_mode(cfg, index, EmptyDirMode::Ignore)
}

fn run_mode(cfg: &Config, index: &mut Index, empty_dirs: EmptyDirMode) -> SyncReport {
    let sync = &cfg.syncs[0];
    let remote = cfg.remote(&sync.remote).unwrap();
    let backend = backend::build(remote, sync, Path::new("/unused-for-local")).unwrap();
    // Resolve max_file_size the way the daemon does: sync → remote → daemon.
    let max_file_size = sync
        .max_file_size
        .or(remote.max_file_size)
        .unwrap_or(cfg.daemon.max_file_size);
    engine::sync_group(
        sync,
        backend.as_ref(),
        index,
        false,
        empty_dirs,
        sync.scope,
        max_file_size,
        &cfg.daemon.exclude,
    )
    .expect("sync should succeed")
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
    assert_eq!(report.added.len(), 1);
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
    assert_eq!(report.added.len(), 1);
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
    assert_eq!(report.removed.len(), 2); // one deleted on each side
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
fn local_backend_preserves_mtime_both_ways() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    // Push: the mirror copy must keep the source's modification time.
    write(&d.local.join("f.txt"), "hi");
    set_mtime(&d.local.join("f.txt"), 1_000_000);
    run(&cfg, &mut index);
    let got = FileTime::from_last_modification_time(
        &fs::metadata(d.remote.join("data/f.txt")).unwrap(),
    );
    assert_eq!(got, FileTime::from_unix_time(1_000_000, 0), "push kept mtime");

    // Pull: the local copy must keep the remote file's modification time.
    write(&d.remote.join("data/g.txt"), "yo");
    set_mtime(&d.remote.join("data/g.txt"), 2_000_000);
    run(&cfg, &mut index);
    let got =
        FileTime::from_last_modification_time(&fs::metadata(d.local.join("g.txt")).unwrap());
    assert_eq!(got, FileTime::from_unix_time(2_000_000, 0), "pull kept mtime");
}

#[test]
fn mirror_syncs_empty_dirs_both_ways_and_ignore_does_not() {
    let d = dirs();
    let mut index = Index::default();

    // An empty dir on each side.
    fs::create_dir_all(d.local.join("emptylocal")).unwrap();
    fs::create_dir_all(d.remote.join("data/emptyremote")).unwrap();

    // Default (ignore): empty dirs are neither created nor removed.
    let cfg = config(&d.local, &d.remote, "");
    let report = run(&cfg, &mut index);
    assert!(report.added.is_empty());
    assert!(!d.remote.join("data/emptylocal").exists());
    assert!(!d.local.join("emptyremote").exists());

    // mirror: each empty dir is mirrored to the other side.
    let mut index = Index::default();
    let report = run_mode(&cfg, &mut index, EmptyDirMode::Mirror);
    assert_eq!(report.added.len(), 2);
    assert!(d.remote.join("data/emptylocal").is_dir());
    assert!(d.local.join("emptyremote").is_dir());
}

#[test]
fn mirror_propagates_empty_dir_deletion() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    fs::create_dir_all(d.local.join("shared")).unwrap();
    run_mode(&cfg, &mut index, EmptyDirMode::Mirror);
    assert!(d.remote.join("data/shared").is_dir());

    // Remove it locally; mirror mode should delete it on the remote too.
    fs::remove_dir(d.local.join("shared")).unwrap();
    let report = run_mode(&cfg, &mut index, EmptyDirMode::Mirror);
    assert_eq!(report.removed.len(), 1);
    assert!(!d.remote.join("data/shared").exists());
}

#[test]
fn prune_removes_empty_dirs_including_nested_parents() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "");
    let mut index = Index::default();

    // A file whose deletion leaves an empty nested dir tree behind.
    write(&d.local.join("a/b/c.txt"), "x");
    run(&cfg, &mut index);
    assert!(d.remote.join("data/a/b/c.txt").exists());

    fs::remove_file(d.local.join("a/b/c.txt")).unwrap();
    let report = run_mode(&cfg, &mut index, EmptyDirMode::Prune);
    // The file delete propagates, and the now-empty a/ and a/b/ are pruned both sides.
    // the file delete plus the now-empty a/ and a/b/ all count as removed
    assert!(report.removed.len() >= 3);
    assert!(!d.local.join("a").exists(), "local empty parents pruned");
    assert!(!d.remote.join("data/a").exists(), "remote empty parents pruned");
}

#[test]
fn include_restricts_what_is_pushed() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"include = ["keep"]"#);
    let mut index = Index::default();

    write(&d.local.join("keep/a.txt"), "yes");
    write(&d.local.join("other/b.txt"), "no");
    let report = run(&cfg, &mut index);

    assert_eq!(report.added.len(), 1);
    assert!(d.remote.join("data/keep/a.txt").exists());
    assert!(
        !d.remote.join("data/other/b.txt").exists(),
        "non-included paths must not be pushed"
    );
}

#[test]
fn scope_include_ignores_paths_outside_include_in_both_directions() {
    // The safe default: an empty `include` syncs nothing at all, so a remote
    // tree cannot delete or overwrite the local one.
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"scope = "include""#);
    let mut index = Index::default();

    write(&d.local.join("mine.txt"), "local");
    write(&d.remote.join("data/theirs.txt"), "remote");
    let report = run(&cfg, &mut index);

    assert!(report.is_empty(), "nothing is included, so nothing syncs");
    assert!(d.local.join("mine.txt").exists(), "local file untouched");
    assert!(!d.local.join("theirs.txt").exists(), "remote file not pulled");
    assert!(
        !d.remote.join("data/mine.txt").exists(),
        "local file not pushed"
    );
}

#[test]
fn scope_include_with_glob_syncs_only_matched_paths_both_ways() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"include = ["keep"]"#);
    let mut index = Index::default();

    // A baseline where `keep/` is synced both ways and `other/` is ignored.
    write(&d.local.join("keep/a.txt"), "yes");
    write(&d.remote.join("data/keep/b.txt"), "also");
    write(&d.local.join("other/x.txt"), "ignore-me");
    write(&d.remote.join("data/other/y.txt"), "ignore-me-too");
    run(&cfg, &mut index);

    assert_eq!(read(&d.remote.join("data/keep/a.txt")), "yes");
    assert_eq!(read(&d.local.join("keep/b.txt")), "also");
    assert!(!d.remote.join("data/other/x.txt").exists());
    assert!(!d.local.join("other/y.txt").exists());

    // A delete inside `include` propagates; one outside is ignored.
    fs::remove_file(d.local.join("keep/a.txt")).unwrap();
    fs::remove_file(d.remote.join("data/other/y.txt")).unwrap();
    run(&cfg, &mut index);
    assert!(!d.remote.join("data/keep/a.txt").exists(), "included delete propagates");
}

#[test]
fn scope_remote_pulls_non_included_files_read_only() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"scope = "remote""#);
    let mut index = Index::default();

    // A non-included remote file is pulled down...
    write(&d.remote.join("data/dotfile"), "from-remote");
    // ...while a non-included local-only file stays put (never pushed).
    write(&d.local.join("local-only.txt"), "mine");
    let report = run(&cfg, &mut index);

    assert_eq!(read(&d.local.join("dotfile")), "from-remote");
    assert_eq!(report.added.len(), 1, "only the remote file is pulled");
    assert!(
        !d.remote.join("data/local-only.txt").exists(),
        "local-only file must not be pushed under scope = remote"
    );

    // Editing the pulled file locally does not push it back: the remote wins,
    // and the diverging local edit is preserved as a conflict copy.
    write(&d.local.join("dotfile"), "local-edit");
    write(&d.remote.join("data/dotfile"), "remote-edit");
    let report = run(&cfg, &mut index);
    assert_eq!(read(&d.local.join("dotfile")), "remote-edit", "remote overrides local");
    assert_eq!(report.conflicts.len(), 1);
    let copy = find_conflict_copy(&d.local).expect("conflict copy should exist");
    assert_eq!(read(&copy), "local-edit");
}

#[test]
fn scope_local_pushes_non_included_files_and_local_overrides_remote() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"scope = "local""#);
    let mut index = Index::default();

    // A non-included local file is pushed up...
    write(&d.local.join("note.txt"), "from-local");
    // ...while a non-included remote-only file stays put (never pulled).
    write(&d.remote.join("data/remote-only.txt"), "theirs");
    let report = run(&cfg, &mut index);

    assert_eq!(read(&d.remote.join("data/note.txt")), "from-local");
    assert_eq!(report.added.len(), 1, "only the local file is pushed");
    assert!(
        !d.local.join("remote-only.txt").exists(),
        "remote-only file must not be pulled under scope = local"
    );

    // Diverging edits: local wins, and the losing remote version is preserved
    // as a local conflict copy.
    write(&d.local.join("note.txt"), "local-edit");
    write(&d.remote.join("data/note.txt"), "remote-edit");
    let report = run(&cfg, &mut index);
    assert_eq!(read(&d.remote.join("data/note.txt")), "local-edit", "local overrides remote");
    assert_eq!(report.conflicts.len(), 1);
    let copy = find_conflict_copy(&d.local).expect("conflict copy should exist");
    assert_eq!(read(&copy), "remote-edit");
}

#[test]
fn scope_mirror_syncs_everything_ignoring_include() {
    // `include` is set but ignored under `scope = "mirror"`: everything syncs.
    let d = dirs();
    let cfg = config(&d.local, &d.remote, "scope = \"mirror\"\ninclude = [\"keep\"]");
    let mut index = Index::default();

    write(&d.local.join("keep/a.txt"), "yes");
    write(&d.local.join("other/b.txt"), "also");
    let report = run(&cfg, &mut index);

    assert_eq!(report.added.len(), 2, "mirror ignores include and syncs all");
    assert!(d.remote.join("data/keep/a.txt").exists());
    assert!(d.remote.join("data/other/b.txt").exists());
}

#[test]
fn max_file_size_skips_oversize_files_in_both_directions() {
    // A 10-byte cap: small files sync, larger ones are skipped either way.
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"max_file_size = "10""#);
    let mut index = Index::default();

    write(&d.local.join("small.txt"), "hi"); // 2 bytes: pushed
    write(&d.local.join("big.txt"), "this is definitely over ten bytes"); // skipped
    write(&d.remote.join("data/incoming-small.txt"), "ok"); // pulled
    write(&d.remote.join("data/incoming-big.txt"), "also way over the ten byte cap"); // skipped
    let report = run(&cfg, &mut index);

    assert_eq!(report.added.len(), 2, "only the two small files sync");
    assert!(d.remote.join("data/small.txt").exists(), "small local file pushed");
    assert!(d.local.join("incoming-small.txt").exists(), "small remote file pulled");
    assert!(
        !d.remote.join("data/big.txt").exists(),
        "oversize local file must not be pushed"
    );
    assert!(
        !d.local.join("incoming-big.txt").exists(),
        "oversize remote file must not be pulled"
    );
}

#[test]
fn max_file_size_zero_means_unlimited() {
    let d = dirs();
    let cfg = config(&d.local, &d.remote, r#"max_file_size = 0"#);
    let mut index = Index::default();

    write(&d.local.join("big.txt"), "content well beyond any tiny cap");
    let report = run(&cfg, &mut index);

    assert_eq!(report.added.len(), 1);
    assert!(d.remote.join("data/big.txt").exists(), "0 = unlimited, so it syncs");
}
