# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-07-11

First public release. A single `conflux` binary runs both the background
sync daemon and the CLI that controls it.

### Added

- **Multiple backends** — sync against WebDAV (Nextcloud & friends), Git (over
  HTTPS or SSH), or a `filesystem` directory (backup disk, testing).
- **`remote_fs` shortcut** — point a `[[sync]]` at a filesystem path directly,
  without declaring a `[[remote]]`; conflux synthesizes a `filesystem` remote
  for it. Supports `trigger = "watch-both"`.
- **Flexible triggers** — `watch` (debounced local edits), `timer` (fixed
  interval), `watch-both` (also watches a filesystem remote), and `manual`.
- **Scoped syncing** — `include` globs pick what syncs two-way, and `scope`
  (`include` / `remote` / `local` / `mirror`) governs everything else, so
  pointing a remote at `$HOME` can't clobber untracked files.
- **Safe by default** — deletions denied unless opted in, a configurable
  `max_file_size`, and diverging edits kept as local conflict copies rather
  than overwritten.
- **Periodic remote pulls** — a `pull_interval` catches changes made on other
  hosts even for `watch`-triggered groups.
- **Profiles** — one shared config drives many hosts; each `--profile` gets its
  own state dir and control socket and runs only its own sync groups.
- **systemd integration** — plain and profile-templated user and system units,
  with content-addressed (BLAKE3) change detection under the hood.
- **CLI** — `status`, `sync`, `reload`, and `config {validate,show,paths}`,
  talking to the daemon over its control socket.
- **Packaging** — Arch Linux `PKGBUILD` and a source install script
  (`install.sh`) for user and system installs.

[1.0.0]: https://github.com/SelfRef/conflux/releases/tag/v1.0.0
