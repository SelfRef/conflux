//! Git backend integration test against a local bare repo (no network).

use conflux_core::backend;
use conflux_core::engine::{self, SyncReport};
use conflux_core::index::Index;
use conflux_core::Config;
use git2::{Repository, Signature};
use std::fs;
use std::path::Path;

/// Initialize a bare repo with an empty initial commit on `main`.
fn seed_bare(path: &Path) {
    let repo = Repository::init_bare(path).unwrap();
    let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = Signature::now("seed", "seed@localhost").unwrap();
    repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
}

fn config(local: &Path, bare: &Path) -> Config {
    let toml = format!(
        r#"
        [[remote]]
        id = "repo"
        backend = "git"
        url = "{bare}"
        branch = "main"

        [[sync]]
        remote = "repo"
        local = "{local}"
        remote_path = "cfg"
        trigger = "manual"
        "#,
        bare = bare.display(),
        local = local.display(),
    );
    Config::from_toml_str(&toml).expect("config should parse")
}

fn run(cfg: &Config, index: &mut Index, state: &Path) -> SyncReport {
    let sync = &cfg.syncs[0];
    let remote = cfg.remote(sync.remote_id()).unwrap();
    let backend = backend::build(remote, sync, state).expect("backend should build");
    engine::sync_group(
        sync,
        backend.as_ref(),
        index,
        false,
        conflux_core::model::EmptyDirMode::Ignore,
        conflux_core::model::Scope::Mirror,
        conflux_core::model::Deletions::Allow,
        0,
        &[],
    )
    .expect("sync should succeed")
}

#[test]
fn commit_msg_command_customizes_the_message() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("remote.git");
    seed_bare(&bare);
    let root = tmp.path().join("a");
    let state = tmp.path().join("state");
    fs::create_dir_all(&root).unwrap();

    let toml = format!(
        r#"
        [[remote]]
        id = "repo"
        backend = "git"
        url = "{bare}"
        branch = "main"
        commit_msg_command = "printf 'custom: %s' hello"

        [[sync]]
        remote = "repo"
        local = "{local}"
        remote_path = "cfg"
        trigger = "manual"
        "#,
        bare = bare.display(),
        local = root.display(),
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let mut index = Index::default();

    fs::write(root.join("f.txt"), b"x").unwrap();
    assert_eq!(run(&cfg, &mut index, &state).added.len(), 1);

    // The pushed commit on the bare repo uses the command's output.
    let repo = Repository::open_bare(&bare).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(commit.message().unwrap(), "custom: hello");
}

#[test]
fn git_round_trip_push_pull_update_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("remote.git");
    seed_bare(&bare);

    // Two "machines", each with its own clone (state dir) and local root.
    let (root_a, state_a) = (tmp.path().join("a"), tmp.path().join("state_a"));
    let (root_b, state_b) = (tmp.path().join("b"), tmp.path().join("state_b"));
    fs::create_dir_all(&root_a).unwrap();
    fs::create_dir_all(&root_b).unwrap();
    let cfg_a = config(&root_a, &bare);
    let cfg_b = config(&root_b, &bare);
    let mut index_a = Index::default();
    let mut index_b = Index::default();

    // A creates a file -> commit + push.
    fs::write(root_a.join("app.conf"), b"v1").unwrap();
    let report = run(&cfg_a, &mut index_a, &state_a);
    assert_eq!(report.added.len(), 1, "A adds the new file");

    // B clones and pulls it.
    let report = run(&cfg_b, &mut index_b, &state_b);
    assert_eq!(report.added.len(), 1, "B pulls the new file (added)");
    assert_eq!(fs::read_to_string(root_b.join("app.conf")).unwrap(), "v1");

    // A updates it -> new commit + push.
    fs::write(root_a.join("app.conf"), b"v2").unwrap();
    let report = run(&cfg_a, &mut index_a, &state_a);
    assert_eq!(report.modified.len(), 1, "A pushes the update (modified)");

    // B pulls the update.
    let report = run(&cfg_b, &mut index_b, &state_b);
    assert_eq!(report.modified.len(), 1, "B pulls the update (modified)");
    assert_eq!(fs::read_to_string(root_b.join("app.conf")).unwrap(), "v2");

    // A deletes it -> deletion is committed + pushed.
    fs::remove_file(root_a.join("app.conf")).unwrap();
    let report = run(&cfg_a, &mut index_a, &state_a);
    assert_eq!(report.removed.len(), 1, "A removes on the remote");

    // B sees the deletion and removes its local copy.
    let report = run(&cfg_b, &mut index_b, &state_b);
    assert_eq!(report.removed.len(), 1, "B removes locally");
    assert!(!root_b.join("app.conf").exists());
}
