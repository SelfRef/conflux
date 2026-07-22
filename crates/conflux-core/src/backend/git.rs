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
    /// SSH credentials to try, in order, for this remote's host. Precomputed
    /// once so the credentials callback can iterate them across libgit2's
    /// retry attempts.
    ssh_creds: Vec<SshCred>,
}

/// One SSH credential candidate the credentials callback can offer.
#[derive(Clone)]
enum SshCred {
    /// Ask a running ssh-agent for a key.
    Agent,
    /// A private key file on disk (from `identity_file`, `~/.ssh/config`, or a
    /// default location).
    KeyFile(PathBuf),
    /// Raw private key material held in memory (from `identity_file_command`).
    KeyInline(String),
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
            ssh_creds: ssh_credentials(remote)?,
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
        // Candidate SSH keys/agent to offer. If none were resolved (e.g. an
        // HTTPS remote, or an SSH host with no keys on disk), fall back to the
        // agent so behaviour is unchanged for the agent-only case.
        let ssh_creds = if self.ssh_creds.is_empty() {
            vec![SshCred::Agent]
        } else {
            self.ssh_creds.clone()
        };
        // libgit2 calls the credentials callback repeatedly, once per auth
        // attempt, until one succeeds (or a credential is reused). We walk
        // `ssh_creds` one entry per SSH_KEY invocation so each key/agent gets a
        // turn instead of retrying the same one forever.
        let mut ssh_attempt = 0usize;
        let mut cb = RemoteCallbacks::new();
        cb.credentials(move |_url, username_from_url, allowed| {
            let ssh_user = username_from_url.unwrap_or("git");
            // Some SSH servers first ask only for the username.
            if allowed.contains(CredentialType::USERNAME) {
                return Cred::username(ssh_user);
            }
            if allowed.contains(CredentialType::SSH_KEY) {
                let attempt = ssh_attempt;
                ssh_attempt += 1;
                return match ssh_creds.get(attempt) {
                    Some(SshCred::Agent) => Cred::ssh_key_from_agent(ssh_user),
                    Some(SshCred::KeyFile(path)) => Cred::ssh_key(ssh_user, None, path, None),
                    Some(SshCred::KeyInline(key)) => {
                        Cred::ssh_key_from_memory(ssh_user, None, key, None)
                    }
                    // Every candidate has been tried; stop retrying so libgit2
                    // surfaces the auth failure instead of looping.
                    None => Err(git2::Error::from_str(
                        "no more SSH credentials to try (checked ssh-agent, \
                         ~/.ssh/config identities, and default keys)",
                    )),
                };
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

/// Build the ordered list of SSH credentials to try for a `remote`.
///
/// libgit2/libssh2 does not read `~/.ssh/config`, so conflux resolves keys
/// itself. Order (most to least specific):
///   1. `identity_file` / `identity_file_command` from the remote's config
///   2. `IdentityFile`s configured for the host in `~/.ssh/config`
///   3. ssh-agent
///   4. default key locations (`~/.ssh/id_ed25519`, `id_rsa`, ...)
///
/// The config-derived entries (2–4) are only added for SSH URLs; explicit
/// deploy keys (1) are honoured regardless. Fails if `identity_file` points at
/// a missing path or `identity_file_command` errors.
fn ssh_credentials(remote: &Remote) -> Result<Vec<SshCred>> {
    let mut seen = std::collections::HashSet::new();
    let mut creds = Vec::new();

    // 1a. Explicit deploy key on disk (highest priority).
    if let Some(raw) = &remote.identity_file {
        let path = PathBuf::from(shellexpand::tilde(raw).into_owned());
        if !path.is_file() {
            return Err(Error::Validation(format!(
                "remote '{}': identity_file not found: {}",
                remote.id,
                path.display()
            )));
        }
        if seen.insert(path.clone()) {
            creds.push(SshCred::KeyFile(path));
        }
    }
    // 1b. Deploy key fetched via command (raw key material).
    if let Some(key) = remote.resolve_identity_file_command()? {
        creds.push(SshCred::KeyInline(key));
    }

    // The remaining candidates only apply to SSH remotes.
    if let Some(host) = ssh_host_from_url(&remote.url) {
        // 2. Keys the user picked for this host in ~/.ssh/config.
        for path in ssh_config_identity_files(&host) {
            if path.is_file() && seen.insert(path.clone()) {
                creds.push(SshCred::KeyFile(path));
            }
        }
        // 3. ssh-agent (keeps the previous behaviour working unchanged).
        creds.push(SshCred::Agent);
        // 4. Default key locations that actually exist.
        for path in default_identity_files() {
            if path.is_file() && seen.insert(path.clone()) {
                creds.push(SshCred::KeyFile(path));
            }
        }
    }
    Ok(creds)
}

/// Extract the SSH host from a git remote URL, or `None` if it isn't SSH.
/// Handles `ssh://[user@]host[:port]/path` and scp-like `[user@]host:path`.
fn ssh_host_from_url(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("ssh://") {
        let authority = rest.split('/').next().unwrap_or(rest);
        return Some(strip_host(authority));
    }
    // Any other explicit scheme (https://, http://, git://, file://) is not SSH.
    if url.contains("://") {
        return None;
    }
    // scp-like syntax: user@host:path — the host is everything before the
    // first colon, which must not contain a path separator.
    if let Some((authority, _)) = url.split_once(':') {
        if !authority.is_empty() && !authority.contains('/') {
            return Some(strip_host(authority));
        }
    }
    None
}

/// Reduce a URL authority (`[user@]host[:port]`, incl. `[ipv6]:port`) to the
/// bare host.
fn strip_host(authority: &str) -> String {
    // Drop any `user@` prefix (rsplit so an `@` in userinfo can't confuse it).
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // Bracketed IPv6 literal: return what's inside the brackets.
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    // Strip a trailing `:port` (only when it's numeric, so we don't mangle a
    // bare IPv6 address that has no brackets).
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            h.to_string()
        }
        _ => host.to_string(),
    }
}

/// The `IdentityFile` paths that apply to `host`, read from `~/.ssh/config`
/// with `~` expanded. Empty if the file is missing or unreadable.
fn ssh_config_identity_files(host: &str) -> Vec<PathBuf> {
    let path = shellexpand::tilde("~/.ssh/config").into_owned();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_identity_files(&text, host)
        .into_iter()
        .map(|f| PathBuf::from(shellexpand::tilde(&f).into_owned()))
        .collect()
}

/// Parse an ssh_config `text` and return the raw (un-expanded) `IdentityFile`
/// tokens that apply to `host`.
///
/// Supports `Host` blocks with `*`/`?` wildcards and `!` negation, and options
/// before the first `Host` (which apply to every host). Other keywords —
/// `Match`, `Include`, `Hostname`, etc. — are ignored.
fn parse_identity_files(text: &str, host: &str) -> Vec<String> {
    let mut files = Vec::new();
    // Options before the first `Host` line apply to all hosts.
    let mut matching = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = split_config_line(line);
        match keyword.to_ascii_lowercase().as_str() {
            "host" => matching = host_matches(host, rest),
            "identityfile" if matching => {
                files.extend(rest.split_whitespace().map(str::to_string));
            }
            _ => {}
        }
    }
    files
}

/// Split an ssh_config line into `(keyword, rest)`, allowing either whitespace
/// or `=` (optionally surrounded by spaces) as the separator.
fn split_config_line(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = &line[..end];
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').map(str::trim_start).unwrap_or(rest);
    (keyword, rest)
}

/// Whether `host` matches any positive pattern in a `Host` line's `patterns`
/// while matching no negated (`!`) pattern.
fn host_matches(host: &str, patterns: &str) -> bool {
    let mut matched = false;
    for pat in patterns.split_whitespace() {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(neg, host) {
                return false;
            }
        } else if glob_match(pat, host) {
            matched = true;
        }
    }
    matched
}

/// Match `text` against an ssh_config host `pattern` supporting `*` (any run)
/// and `?` (single char). Case-insensitive, like OpenSSH.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let txt: Vec<char> = text.to_ascii_lowercase().chars().collect();
    // Iterative wildcard match with backtracking on `*`.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// Default private-key locations OpenSSH would try, in preference order.
fn default_identity_files() -> Vec<PathBuf> {
    [
        "id_ed25519",
        "id_ecdsa",
        "id_ecdsa_sk",
        "id_ed25519_sk",
        "id_rsa",
        "id_dsa",
    ]
    .iter()
    .map(|name| PathBuf::from(shellexpand::tilde(&format!("~/.ssh/{name}")).into_owned()))
    .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ssh_host_from_url_forms() {
        assert_eq!(
            ssh_host_from_url("ssh://git@git.aperte.dev:222/SelfRef/test.git").as_deref(),
            Some("git.aperte.dev")
        );
        assert_eq!(
            ssh_host_from_url("ssh://git.example.com/repo.git").as_deref(),
            Some("git.example.com")
        );
        // scp-like syntax.
        assert_eq!(
            ssh_host_from_url("git@github.com:me/dots.git").as_deref(),
            Some("github.com")
        );
        // Bracketed IPv6 with a port.
        assert_eq!(
            ssh_host_from_url("ssh://git@[2001:db8::1]:2222/repo.git").as_deref(),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn non_ssh_urls_have_no_host() {
        assert_eq!(ssh_host_from_url("https://github.com/me/dots.git"), None);
        assert_eq!(ssh_host_from_url("http://example.com/repo.git"), None);
        assert_eq!(ssh_host_from_url("git://example.com/repo.git"), None);
        assert_eq!(ssh_host_from_url("file:///srv/repo.git"), None);
        // A bare local path is not an scp target.
        assert_eq!(ssh_host_from_url("/srv/git/repo.git"), None);
    }

    #[test]
    fn glob_matches_wildcards() {
        assert!(glob_match("git.aperte.dev", "git.aperte.dev"));
        assert!(glob_match("*.aperte.dev", "git.aperte.dev"));
        assert!(glob_match("git.*", "git.aperte.dev"));
        assert!(glob_match("*", "anything.at.all"));
        assert!(glob_match("git?.example.com", "git1.example.com"));
        assert!(!glob_match("git?.example.com", "git.example.com"));
        assert!(!glob_match("*.aperte.dev", "git.example.com"));
        // Case-insensitive, like OpenSSH.
        assert!(glob_match("Git.Aperte.Dev", "git.aperte.dev"));
    }

    #[test]
    fn host_line_negation_excludes() {
        assert!(host_matches("git.aperte.dev", "*.aperte.dev"));
        assert!(!host_matches(
            "secret.aperte.dev",
            "*.aperte.dev !secret.aperte.dev"
        ));
        assert!(host_matches(
            "public.aperte.dev",
            "*.aperte.dev !secret.aperte.dev"
        ));
    }

    #[test]
    fn parses_identity_file_for_matching_host() {
        let config = "\
# a comment
Host github.com
    IdentityFile ~/.ssh/github_key

Host git.aperte.dev
    User git
    IdentityFile ~/.ssh/gitea_selfref
";
        assert_eq!(
            parse_identity_files(config, "git.aperte.dev"),
            vec!["~/.ssh/gitea_selfref"]
        );
        assert_eq!(
            parse_identity_files(config, "github.com"),
            vec!["~/.ssh/github_key"]
        );
        // A host with no matching block gets nothing.
        assert!(parse_identity_files(config, "unknown.example.com").is_empty());
    }

    #[test]
    fn parses_wildcard_and_multiple_identity_files() {
        let config = "\
Host *.aperte.dev
    IdentityFile ~/.ssh/aperte_a
    IdentityFile ~/.ssh/aperte_b
";
        assert_eq!(
            parse_identity_files(config, "git.aperte.dev"),
            vec!["~/.ssh/aperte_a", "~/.ssh/aperte_b"]
        );
    }

    #[test]
    fn parses_equals_separator_and_global_options() {
        // Options before the first `Host` apply to every host; `=` is a valid
        // separator.
        let config = "\
IdentityFile=~/.ssh/global_key

Host git.aperte.dev
    IdentityFile = ~/.ssh/host_key
";
        assert_eq!(
            parse_identity_files(config, "git.aperte.dev"),
            vec!["~/.ssh/global_key", "~/.ssh/host_key"]
        );
        assert_eq!(
            parse_identity_files(config, "other.example.com"),
            vec!["~/.ssh/global_key"]
        );
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        let config = "\
host git.aperte.dev
    identityfile ~/.ssh/gitea_selfref
";
        assert_eq!(
            parse_identity_files(config, "git.aperte.dev"),
            vec!["~/.ssh/gitea_selfref"]
        );
    }
}
