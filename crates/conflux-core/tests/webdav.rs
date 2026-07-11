//! Live WebDAV integration test. Skipped unless `CONFLUX_WEBDAV_URL` is set.
//!
//! Run against a throwaway server, e.g.:
//!   docker run --rm -p 4918:8080 -v /tmp/dav:/data rclone/rclone \
//!     serve webdav /data --addr :8080 --user test --pass test123
//!   CONFLUX_WEBDAV_URL=http://localhost:4918/ CONFLUX_WEBDAV_USER=test \
//!     CONFLUX_WEBDAV_PASS=test123 cargo test -p conflux-core --test webdav -- --nocapture

use conflux_core::backend;
use conflux_core::engine;
use conflux_core::index::Index;
use conflux_core::Config;
use std::fs;
use std::path::Path;

fn env() -> Option<(String, String, String)> {
    let url = std::env::var("CONFLUX_WEBDAV_URL").ok()?;
    let user = std::env::var("CONFLUX_WEBDAV_USER").unwrap_or_default();
    let pass = std::env::var("CONFLUX_WEBDAV_PASS").unwrap_or_default();
    Some((url, user, pass))
}

fn config(local: &Path, url: &str, user: &str, pass: &str) -> Config {
    let toml = format!(
        r#"
        [[remote]]
        id = "dav"
        backend = "webdav"
        url = "{url}"
        username = "{user}"
        password = "{pass}"

        [[sync]]
        remote = "dav"
        local = "{local}"
        remote_path = "conflux-test"
        trigger = "manual"
        "#,
        local = local.display(),
    );
    Config::from_toml_str(&toml).expect("config should parse")
}

fn run(cfg: &Config, index: &mut Index) -> engine::SyncReport {
    let sync = &cfg.syncs[0];
    let remote = cfg.remote(&sync.remote).unwrap();
    let backend = backend::build(remote, sync, Path::new("/unused-for-webdav"))
        .expect("backend should build");
    engine::sync_group(sync, backend.as_ref(), index, false, conflux_core::model::EmptyDirMode::Ignore, conflux_core::model::Scope::Mirror, 0, &[]).expect("sync should succeed")
}

#[test]
fn webdav_round_trip_push_pull_delete() {
    let Some((url, user, pass)) = env() else {
        eprintln!("skipping: CONFLUX_WEBDAV_URL not set");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    fs::create_dir_all(&root_a).unwrap();
    fs::create_dir_all(&root_b).unwrap();

    // A pushes a nested file (exercises MKCOL of parent collections).
    let cfg_a = config(&root_a, &url, &user, &pass);
    let mut index_a = Index::default();
    fs::create_dir_all(root_a.join("sub")).unwrap();
    fs::write(root_a.join("sub/notes.txt"), b"hello dav").unwrap();
    let report = run(&cfg_a, &mut index_a);
    assert_eq!(report.added.len(), 1, "A should add one file");

    // B (fresh) pulls the same remote path.
    let cfg_b = config(&root_b, &url, &user, &pass);
    let mut index_b = Index::default();
    let report = run(&cfg_b, &mut index_b);
    assert_eq!(report.added.len(), 1, "B should pull one new file (added)");
    assert_eq!(
        fs::read_to_string(root_b.join("sub/notes.txt")).unwrap(),
        "hello dav"
    );

    // A deletes it; the delete should propagate to the remote.
    fs::remove_file(root_a.join("sub/notes.txt")).unwrap();
    let report = run(&cfg_a, &mut index_a);
    assert_eq!(report.removed.len(), 1, "A should remove on remote");

    // B sees the remote deletion and removes its local copy.
    let report = run(&cfg_b, &mut index_b);
    assert_eq!(report.removed.len(), 1, "B should remove locally");
    assert!(!root_b.join("sub/notes.txt").exists());
}
