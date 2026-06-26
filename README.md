# conflux

A small Linux background service that syncs files — mainly `~/.config` files and
scripts — between your machine and one or more remotes. Bidirectional by default,
with **newer-wins** conflict resolution that never throws away data.

- **Backends:** WebDAV (e.g. Nextcloud), git, and a local-directory mirror.
- **Triggers:** manual, on a timer, or on file changes (debounced).
- **One binary:** `conflux daemon` is the long-running service; the other
  subcommands are CLI control that talk to it over a Unix socket.
- **Runs per-user** (`systemd --user`) **or system-wide.**

> Status: a complete, tested implementation of milestones M0–M5. This is a
> learning project; the code favors clarity over cleverness.

## Install

```sh
cargo build --release
install -Dm755 target/release/conflux ~/.local/bin/conflux   # or /usr/local/bin
```

## Configure

Config is TOML. Default locations (override with `--config` or `$CONFLUX_CONFIG`):

| | user mode | system mode (`--system`) |
|---|---|---|
| config | `~/.config/conflux/config.toml` | `/etc/conflux/config.toml` |
| state/index | `~/.local/state/conflux/` | `/var/lib/conflux/` |
| socket | `$XDG_RUNTIME_DIR/conflux.sock` | `/run/conflux/conflux.sock` |

See [`config.example.toml`](config.example.toml) for a fully-commented example.
A minimal config:

```toml
[[remote]]
name = "nextcloud"
type = "webdav"
url  = "https://cloud.example.com/remote.php/dav/files/me/"
username = "me"
password = "secret"            # or: password_command = "secret-tool lookup ..."

[[sync]]
remote      = "nextcloud"
root        = "~/.config"      # one local root
remote_path = "config"
include     = ["nvim", "fish"] # only these are pushed (default: everything)
trigger     = "watch"          # manual | timer | watch
```

Validate it before starting the daemon:

```sh
conflux config validate
conflux config show       # print the parsed config
conflux config paths      # show resolved config/state/socket paths
```

## Run

```sh
# Foreground (for trying it out)
conflux daemon

# Or as a user service
cp systemd/conflux.user.service ~/.config/systemd/user/conflux.service
systemctl --user daemon-reload
systemctl --user enable --now conflux
journalctl --user -u conflux -f
```

Control the running daemon:

```sh
conflux status                 # per-group last-run summary
conflux sync                   # sync everything now
conflux sync nextcloud:config  # sync one group (label is remote:remote_path)
conflux sync --profile desktop # sync a profile
conflux reload                 # re-read the config file (also: systemctl reload / SIGHUP)
```

## Key concepts

- **Sync group** — one local `root` mapped to a remote's `remote_path`. Its label
  is `remote:remote_path`.
- **`include` (push scope)** — globs (relative to `root`) selecting what is
  *pushed*. Empty means everything. Pull always downloads the whole remote tree
  unless `pull_scope = "include"`.
- **`direction`** — `sync` (bidirectional, default) or `pull` (download only;
  local changes are never uploaded).
- **Conflicts (newer-wins)** — if a file changed on both sides, the newer mtime
  wins and the losing version is preserved next to it as
  `name.conflux-conflict-<epoch>.ext` (and logged). These copies are never synced.
- **Profiles** — tag groups with `profiles = ["desktop", ...]`. `daemon.active_profile`
  selects which groups this host runs; unset/`default` runs all.
- **Triggers** — `manual` (only `conflux sync`), `timer` (`interval`, default 1h),
  `watch` (inotify, debounced by `debounce`: per-sync → per-remote → daemon default 2s).

## Architecture

A Cargo workspace:

- **`conflux-core`** — the synchronous core: config, model, the reconciliation
  `engine`, the per-group baseline `index`, and the `Backend` trait with `local`,
  `webdav`, and `git` implementations.
- **`conflux`** — the binary. `conflux daemon` is the only async part: a
  single-worker scheduler driven by timer/watch/IPC triggers. Each sync runs on a
  blocking thread and is serialized daemon-wide, so the engine is plain blocking
  code and never races on a group's index.

The engine does a three-way diff of the current local tree, the current remote
snapshot, and the baseline index, then pushes/pulls/deletes per file and resolves
conflicts. Remote change-detection ids are: WebDAV ETag, git blob-content hash,
local content hash.

## Caveats

- **git mtimes are approximate.** git stores no per-file mtime, so newer-wins for
  the git backend uses the repository's HEAD commit time for every file.
- **Delete handling is conservative.** If one side deletes a file the other side
  modified, the modification wins (the file is resurrected) and it's logged.
- The baseline index is JSON per group under the state dir; fine for typical
  dotfile/script trees.

## Development

```sh
cargo test                       # unit + engine + git integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Two integration tests reach external services and are opt-in:

```sh
# git: uses a local bare repo, always runs (no network)
cargo test -p conflux-core --test git

# webdav: needs a server; e.g. with docker + rclone:
docker run --rm -p 4918:8080 -v /tmp/dav:/data rclone/rclone \
  serve webdav /data --addr :8080 --user test --pass test123
CONFLUX_WEBDAV_URL=http://localhost:4918/ CONFLUX_WEBDAV_USER=test \
  CONFLUX_WEBDAV_PASS=test123 cargo test -p conflux-core --test webdav
```

## License

MIT
