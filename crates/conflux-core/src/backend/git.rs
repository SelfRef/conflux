//! A git backend: clone/pull a repo, mirror a group into a subdirectory of the
//! working tree, then commit and push on `finalize`.
//!
//! Unlike WebDAV's per-file model, git is repo-oriented:
//! - `snapshot` fetches and hard-resets the local clone to the remote branch,
//!   then reads the working tree (id = blake3 content hash).
//! - `read`/`write`/`remove` operate on the working tree under `remote_path`.
//! - `finalize` stages everything (including deletions), commits, and pushes.
//!
//! git stores no per-file mtime, so newer-wins uses the **HEAD commit time** as
//! the mtime for every remote file — an approximation documented in the README.

use super::{walk_snapshot, Backend, RemoteSnapshot};
use crate::config::{Remote, Sync};
use crate::error::{Error, Result};
use crate::hash::hash_bytes;
use crate::model::RemoteMeta;
use crate::relpath::RelPath;
use git2::{
    Cred, CredentialType, FetchOptions, IndexAddOption, PushOptions, RemoteCallbacks, Repository,
    ResetType, Signature,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A git-backed remote. The clone lives at `<state_dir>/git/<remote-name>`.
pub struct GitBackend {
    repo_dir: PathBuf,
    base: PathBuf,
    url: String,
    branch: Option<String>,
    username: Option<String>,
    password: Option<String>,
    commit_msg_command: Option<String>,
}

impl GitBackend {
    /// Build the backend, resolving credentials (may run `password_command`).
    pub fn new(remote: &Remote, sync: &Sync, state_dir: &Path) -> Result<Self> {
        let repo_dir = state_dir.join("git").join(sanitize(&remote.id));
        let mut base = repo_dir.clone();
        for part in sync.remote_path.split('/').filter(|s| !s.is_empty()) {
            base.push(part);
        }
        Ok(GitBackend {
            repo_dir,
            base,
            url: remote.url.clone(),
            branch: remote.branch.clone(),
            username: remote.username.clone(),
            password: remote.resolve_password()?,
            commit_msg_command: remote.commit_msg_command.clone(),
        })
    }

    /// The commit message: `commit_msg_command`'s stdout (run via `sh -c` in the
    /// clone, so it can inspect the staged index), else the default
    /// `"conflux sync from <hostname>"`.
    fn commit_message(&self) -> Result<String> {
        let Some(cmd) = &self.commit_msg_command else {
            return Ok(default_commit_message());
        };
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.repo_dir)
            .output()
            .map_err(|e| Error::Backend(format!("commit_msg_command failed to run: {e}")))?;
        if !output.status.success() {
            return Err(Error::Backend(format!(
                "commit_msg_command exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let msg = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        // A git commit message must not be empty; fall back if the command
        // produced nothing.
        Ok(if msg.is_empty() {
            default_commit_message()
        } else {
            msg
        })
    }

    fn full(&self, path: &RelPath) -> PathBuf {
        path.to_local(&self.base)
    }

    fn callbacks(&self) -> RemoteCallbacks<'_> {
        let user = self.username.clone();
        let pass = self.password.clone();
        let mut cb = RemoteCallbacks::new();
        cb.credentials(move |_url, username_from_url, allowed| {
            if allowed.contains(CredentialType::SSH_KEY) {
                return Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"));
            }
            if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
                if let Some(p) = &pass {
                    return Cred::userpass_plaintext(user.as_deref().unwrap_or(""), p);
                }
            }
            Cred::default()
        });
        cb
    }

    /// Open the existing clone (fetching + resetting to the remote) or clone fresh.
    fn open(&self) -> Result<Repository> {
        if let Ok(repo) = Repository::open(&self.repo_dir) {
            self.fetch_and_reset(&repo)?;
            return Ok(repo);
        }
        if let Some(parent) = self.repo_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(self.callbacks());
        let mut builder = git2::build::RepoBuilder::new();
        builder.fetch_options(fo);
        if let Some(branch) = &self.branch {
            builder.branch(branch);
        }
        builder
            .clone(&self.url, &self.repo_dir)
            .map_err(gerr("clone"))
    }

    fn branch_name(&self, repo: &Repository) -> String {
        if let Some(b) = &self.branch {
            return b.clone();
        }
        repo.head()
            .ok()
            .and_then(|h| h.shorthand().map(str::to_string))
            .unwrap_or_else(|| "main".to_string())
    }

    /// Fetch the branch and hard-reset the working tree to it.
    fn fetch_and_reset(&self, repo: &Repository) -> Result<()> {
        let branch = self.branch_name(repo);
        let mut remote = repo.find_remote("origin").map_err(gerr("find origin"))?;
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(self.callbacks());
        remote
            .fetch(&[&branch], Some(&mut fo), None)
            .map_err(gerr("fetch"))?;

        // If the remote branch exists, hard-reset to it; otherwise the remote is
        // empty and there is nothing to reset to.
        if let Ok(oid) = repo.refname_to_id(&format!("refs/remotes/origin/{branch}")) {
            let object = repo.find_object(oid, None).map_err(gerr("find fetched"))?;
            repo.reset(&object, ResetType::Hard, None)
                .map_err(gerr("reset"))?;
        }
        Ok(())
    }

    fn head_commit_time(repo: &Repository) -> Option<SystemTime> {
        let commit = repo.head().ok()?.peel_to_commit().ok()?;
        let secs = commit.time().seconds();
        u64::try_from(secs)
            .ok()
            .map(|s| UNIX_EPOCH + Duration::from_secs(s))
    }
}

impl Backend for GitBackend {
    fn snapshot(&self) -> Result<RemoteSnapshot> {
        let repo = self.open()?;
        let mtime = Self::head_commit_time(&repo);
        walk_snapshot(&self.base, mtime)
    }

    fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.full(path))?)
    }

    // `_mtime` is ignored: git stores no per-file mtime (it uses commit time).
    fn write(&self, path: &RelPath, data: &[u8], _mtime: Option<SystemTime>) -> Result<RemoteMeta> {
        let full = self.full(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, data)?;
        Ok(RemoteMeta {
            id: hash_bytes(data),
            mtime: None,
            size: data.len() as u64,
        })
    }

    fn remove(&self, path: &RelPath) -> Result<()> {
        match std::fs::remove_file(self.full(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn finalize(&self) -> Result<()> {
        let repo = Repository::open(&self.repo_dir).map_err(gerr("open"))?;
        let mut index = repo.index().map_err(gerr("index"))?;
        // Stage modifications, additions, and deletions (like `git add -A`).
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .map_err(gerr("stage"))?;
        index.write().map_err(gerr("write index"))?;
        let tree_oid = index.write_tree().map_err(gerr("write tree"))?;

        let parent = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok());

        // Nothing changed since HEAD — skip the commit and push.
        if let Some(p) = &parent {
            if p.tree_id() == tree_oid {
                return Ok(());
            }
        }

        let tree = repo.find_tree(tree_oid).map_err(gerr("find tree"))?;
        let sig = Signature::now("conflux", "conflux@localhost").map_err(gerr("signature"))?;
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let message = self.commit_message()?;
        repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &parents)
            .map_err(gerr("commit"))?;

        let branch = self.branch_name(&repo);
        let mut remote = repo.find_remote("origin").map_err(gerr("find origin"))?;
        let mut po = PushOptions::new();
        po.remote_callbacks(self.callbacks());
        remote
            .push(
                &[format!("refs/heads/{branch}:refs/heads/{branch}")],
                Some(&mut po),
            )
            .map_err(gerr("push"))?;
        Ok(())
    }
}

/// Map a git2 error to our backend error, tagging the operation.
fn gerr(op: &'static str) -> impl Fn(git2::Error) -> Error {
    move |e| Error::Backend(format!("git {op} failed: {e}"))
}

/// Make a filesystem-safe directory name from a remote name.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Default commit message, tagged with this host so a shared repo shows which
/// machine pushed. Falls back to a plain message if the hostname is unavailable.
fn default_commit_message() -> String {
    match hostname() {
        Some(h) => format!("conflux sync from {h}"),
        None => "conflux sync".to_string(),
    }
}

/// The system hostname via `gethostname(2)`, or `None` on failure.
fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into `buf` and, on
    // success, NUL-terminates when there is room.
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}
