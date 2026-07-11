# conflux

A background file-sync daemon that keeps local directories in step with a
remote. Point it at a WebDAV server (e.g. Nextcloud), a Git repository, or
another local directory, pick when each group syncs (on file changes, on a
timer, or manually), and how much of the tree it owns — from a read-only pull
of tracked dotfiles to a full 1:1 mirror. A single `conflux` binary runs both
the daemon and the CLI that controls it.

## Features

- **Multiple backends** — sync against **WebDAV** (Nextcloud & friends),
  **Git** (over HTTPS or SSH), or a **local** directory (backup disk, testing).
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
You need at least one `[[remote]]` and one `[[sync]]` for the daemon to do
anything. Validate your config before enabling the daemon:

```sh
conflux config validate
conflux config show       # print the resolved configuration
```

### Example

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
