# conflux

A background file-sync daemon that keeps local directories in step with a
remote. Point it at a WebDAV server (e.g. Nextcloud), a Git repository, or
another local directory, pick when each group syncs (on file changes, on a
timer, or manually), and how much of the tree it owns — from a read-only pull
of tracked dotfiles to a full 1:1 mirror. A single `conflux` binary runs both
the daemon and the CLI that controls it.

## Features

- **Multiple backends** — sync against **WebDAV** (Nextcloud & friends),
  **Git** (over HTTPS or SSH), or a **filesystem** directory (backup disk,
  testing) — the last reachable without a `[[remote]]` via the `remote_fs`
  shortcut.
- **Flexible triggers** — `watch` (react to local edits, debounced), `timer`
  (every interval), `watch-both` (also watch a local remote), or `manual`.
- **Scoped syncing** — `include` globs decide what syncs two-way, and `scope`
  (`include` / `remote` / `local` / `mirror`) decides how everything else is
  treated, so pointing a remote at `$HOME` can't clobber untracked files.
- **Safe by default** — deletions are denied unless opted in, a configurable
  max file size is enforced, and diverging edits are kept as local conflict
  copies rather than overwritten.
- **Periodic remote pulls** — a `pull_interval` catches changes made on other
  hosts even for `watch`-triggered groups.
- **Profiles** — one shared config can drive many hosts; each `--profile` gets
  its own state dir and control socket and runs only its own sync groups.
- **First-class systemd integration** — plain and profile-templated user and
  system units, plus content-addressed (BLAKE3) change detection under the hood.

## Install

### Arch Linux

Three AUR packages are available — pick one:

| Package | Source |
| --- | --- |
| `conflux` | stable release, built from source |
| `conflux-git` | latest `master`, built from source |
| `conflux-bin` | stable release, prebuilt binary from GitHub Releases |

With an AUR helper such as [`yay`](https://github.com/Jguer/yay):

```sh
yay -S conflux        # or conflux-git / conflux-bin
```

Without an AUR helper, clone the package and build it with `makepkg`:

```sh
git clone https://aur.archlinux.org/conflux.git   # or conflux-git / conflux-bin
cd conflux
makepkg -si
```

(The `conflux-git` `PKGBUILD` also lives in this repo under
[`package/arch/`](package/arch/).)

### Local

Build from source and install into your home directory (binary in
`~/.local/bin`, config in `~/.config/conflux/`, systemd **user** units) with the
bundled script:

```sh
./install.sh install
```

To install without rebuilding an existing `target/release/conflux`, add
`--no-build`. To remove it again:

```sh
./install.sh uninstall           # keeps config and state
./install.sh uninstall --purge   # also removes config and state
```

### System

Install system-wide (binary in `/usr/local/bin`, config in `/etc/conflux/`,
systemd **system** units, running as a dedicated `conflux` user) by adding
`--system`; this requires root:

```sh
sudo ./install.sh install --system
```

Both a plain unit and a profile-templated unit are installed in each mode, so
you can enable the implicit `default` profile or a named one:

```sh
sudo systemctl enable --now conflux            # default profile
sudo systemctl enable --now conflux@desktop    # a named profile
```

Uninstall the same way you installed, with `--system` (add `--purge` to also
drop the config, state, and system user):

```sh
sudo ./install.sh uninstall --system
```

## Configure

conflux reads a single TOML config file, resolved automatically:

- **user mode:** `$XDG_CONFIG_HOME/conflux/config.toml` (usually
  `~/.config/conflux/config.toml`)
- **system mode:** `/etc/conflux/config.toml`

Override the path with `$CONFLUX_CONFIG` or `--config <path>`, and print the
resolved locations with `conflux config paths`.

A fully commented [`config.example.toml`](config.example.toml) documents every
option at its default value; the installer drops it in as your starter config.
You need at least one `[[sync]]` for the daemon to do anything — plus a matching
`[[remote]]`, unless the sync uses the `remote_fs` shortcut for a filesystem
target. Validate your config before enabling the daemon:

```sh
conflux config validate
conflux config show       # print the resolved configuration
```

### Examples

A dotfiles Git repo over HTTPS, pulled into `$HOME` read-only with a handful of
files promoted to two-way sync:

```toml
[[remote]]
id = "dotfiles"
backend = "git"
url = "https://github.com/me/dotfiles.git"
branch = "main"
# Auth for HTTPS; prefer a command over a plaintext password.
username = "me"
password_command = "secret-tool lookup service conflux"

[[sync]]
remote = "dotfiles"
local = "~"
# Pull every tracked file down read-only; only the paths in `include`
# are pushed back, so untracked files in $HOME are never touched.
scope = "remote"
include = [".bashrc", ".config/nvim", ".config/fish"]
trigger = "timer"
interval = "15m"
```

A `~/Documents` tree mirrored 1:1 to WebDAV (e.g. Nextcloud), with deletions
propagated both ways so the two copies stay identical:

```toml
[[remote]]
id = "nextcloud"
backend = "webdav"
url = "https://cloud.example.com/remote.php/dav/files/me/"
username = "me"
password_command = "secret-tool lookup service conflux"

[[sync]]
remote = "nextcloud"
local = "~/Documents"
remote_path = "documents"
# Mirror the whole tree, ignoring `include`, and propagate removals: deleting a
# file on one side deletes it on the other. This opts out of the safe default
# (deletions = "deny"), so point it only at a directory you truly want kept
# identical — never at $HOME.
scope = "mirror"
deletions = "allow"
trigger = "watch"
```

A `~/Documents` tree mirrored 1:1 to a local backup disk, with no `[[remote]]`
at all — `remote_fs` points straight at the target directory and conflux
synthesizes a `filesystem` remote for it. `watch-both` reacts to edits on either
side (only a filesystem target can be watched like this):

```toml
[[sync]]
remote_fs = "/mnt/backup/conflux"
local = "~/Documents"
remote_path = "documents"
# Keep both copies identical, deletions included. As with any mirror, point it
# only at a directory you truly want kept in lock-step — never at $HOME.
scope = "mirror"
deletions = "allow"
trigger = "watch-both"
```

The equivalent with an explicit `[[remote]]` (use this when several groups share
one target, or you want to set remote-level `pull_interval`/`max_file_size`):

```toml
[[remote]]
id = "backup"
backend = "filesystem"
url = "/mnt/backup/conflux"

[[sync]]
remote = "backup"
local = "~/Documents"
remote_path = "documents"
scope = "mirror"
deletions = "allow"
trigger = "watch-both"
```

### Environment variables

conflux reads a few variables from the environment (the daemon inherits the
environment of the process that starts it — the systemd unit or your shell):

| Variable | Effect |
| --- | --- |
| `CONFLUX_CONFIG` | Absolute path to the config file, overriding the default location (same as `--config`). Applies to every command. |
| `CONFLUX_LOG` | Tracing filter, e.g. `debug`, `trace`, or a per-target directive like `info,conflux_core=debug`. Overrides `[daemon] log_level` when set. |

In **user mode**, the default config, state, and socket locations follow the
XDG base directories, so the usual XDG variables shift them:

| Variable | Effect |
| --- | --- |
| `XDG_CONFIG_HOME` | Config dir; config file is `$XDG_CONFIG_HOME/conflux/config.toml` (default `~/.config`). |
| `XDG_STATE_HOME` | State dir root (default `~/.local/state`). |
| `XDG_RUNTIME_DIR` | Directory for the control socket; falls back to the state dir if unset. |

Run `conflux config paths` to print the resolved config, state, and socket
locations for the current mode and profile.

Any `password_command` or `commit_msg_command` you configure also runs in this
inherited environment, so it can read your own variables (e.g.
`password_command = 'printf %s "$CONFLUX_PASSWORD"'`).

## Run

### As a daemon (systemd)

If you installed with the script, enable the unit for your mode. For a **user**
install:

```sh
systemctl --user enable --now conflux            # default profile
systemctl --user enable --now conflux@desktop    # a named profile
```

For a **system** install, use the system manager with `sudo`:

```sh
sudo systemctl enable --now conflux
sudo systemctl enable --now conflux@desktop
```

To run the daemon in the foreground instead (for debugging), run
`conflux daemon` directly; set `CONFLUX_LOG=debug` for verbose output.

### CLI commands

The CLI talks to the running daemon over its control socket. Add `--system` for
a system daemon and `--profile <name>` to target a specific profile.

```sh
conflux status              # show each sync group and its last run
conflux sync                # trigger a sync now for every group
conflux sync dotfiles       # trigger just one group (by id or remote:path)
conflux reload              # reload the config without restarting
conflux config validate     # check the config file
conflux config show         # print the resolved config
conflux config paths        # print config/state/socket paths
```
