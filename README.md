# onedrive-rs

Microsoft OneDrive client for Linux, written in Rust: a background service
(`onedrive-rs daemon`, systemd user unit) and a small status window with a tray
icon (`onedrive-rs gui`) talking to it over D-Bus.

It is a rewrite of [abraunegg/onedrive](https://github.com/abraunegg/onedrive)
(the "D client") with the same database schema, configuration format and
synchronisation rules, aiming at a small, predictable memory footprint
(about 25 MB of RAM for tens of thousands of files).

Licence: GPL-3.0-only (derived work of the D client).

## Install

Requires a Rust toolchain (`cargo`). No system development packages are needed.

```sh
contrib/install.sh              # binary in ~/.local/bin, systemd unit, menu entry
contrib/install.sh --autostart  # same, plus the tray icon at login
onedrive-rs login               # opens the browser for Microsoft sign-in
```

## Start syncing

Fresh start (downloads everything into `sync_dir`, default `~/OneDrive-rs`):

```sh
systemctl --user enable --now onedrive-rs
onedrive-rs gui
```

### Switching from the D client

The databases are compatible, so onedrive-rs can take over an existing D client
installation (database, sync cursor and folder) without downloading anything again.
Two clients must never share a sync folder: stop the D client for good first.

```sh
onedrive-rs login                                  # same account as the D client
systemctl --user disable --now onedrive.service    # stop the D client
onedrive-rs import-d --from ~/.config/onedrive     # copies its database, sets sync_dir
onedrive-rs sync --dry-run                         # optional: should report nothing to do
systemctl --user enable --now onedrive-rs
```

`import-d` refuses to run while the D client holds its database. To go back,
stop `onedrive-rs` and re-enable `onedrive.service` (the D client's database is
left untouched).

## Commands

| Command | |
|---|---|
| `login [--manual]` | Microsoft sign-in (loopback redirect; `--manual` to paste the URL) |
| `sync [--mode download-only\|bidirectional] [--dry-run]` | One pass, then exit |
| `daemon` | Periodic passes, local change watching, D-Bus control |
| `gui [--hidden]` | Status window and tray icon |
| `status` | Account, daemon state, database counts |
| `sync-now`, `pause`, `resume` | Control the running daemon |
| `import-d` | Take over a D client installation |

## Configuration

`~/.config/onedrive-rs/config`, same format as the D client (`key = "value"`).
Command-line options take precedence.

| Key | Default |
|---|---|
| `sync_dir` | `~/OneDrive-rs` |
| `download_only` | `false` (bidirectional) |
| `monitor_interval` | `300` seconds between passes |
| `threads` | `8` concurrent transfers |
| `skip_file` | added to `~*\|.~*\|*.tmp\|*.swp\|*.partial` |
| `skip_dir` | none; a name skips it anywhere, a path (`Documents/Old`) only there |
| `skip_dotfiles` | `false` |
| `sync_root_files` | `false`; with a `sync_list`, also sync the files at the top of the sync folder |
| `classify_as_big_delete` | `1000`: refuse to delete more items online in one pass |
| `sync_dir_permissions` / `sync_file_permissions` | `700` / `600` |
| `application_id` | the D client's public Entra application |

A `[drive "name"]` section may set its own `skip_file`, `skip_dir`,
`skip_dotfiles` and `sync_root_files`; they replace the top-level values for
that drive.

### Selective sync

As with the D client, a `sync_list` file next to the config limits the sync to
what it lists (`sync_list.<name>` for a `[drive "name"]`). Everything else is
excluded; exclusion rules win over inclusions:

```text
# The Documents folder at the top, and everything in it
/Documents
# Only this folder inside Teams (Teams itself is created, nothing else in it)
/Teams/Engineering/
# But not this one
!/Documents/Old
# Any folder named node_modules, at any depth
!node_modules/
```

A leading `/` anchors a rule at the top of the sync folder; without it, the
rule matches at any depth (and every folder is then synced, in case it contains
a match). A trailing `/` (or `/*`) only matches folders. `*` and `?` match
within a name, `**` any number of folders.

The GUI writes this file for you: the folder icon of a drive opens a folder
tree to tick (files at the top of the drive then stay synced, through
`sync_root_files`); rules it cannot show are kept as written.

Unlike the D client, no `--resync` is needed after changing `sync_list` or a
`skip_*` option: the next pass notices it, forgets what is now excluded and
enumerates the drive again to bring in what is now included. Local copies of
excluded items are kept (and never uploaded); identical local files are not
downloaded again.

The GUI's Settings window changes `monitor_interval` and the top-level
filters (written to this file, applied without restarting the daemon), and
whether the tray icon starts with the desktop session. Each drive can also be
paused on its own from its card.

Files: `refresh_token` (mode 0600), `items.sqlite3` (D client schema), `lock`,
`upload_sessions/` (interrupted uploads), `filters/` (filters of the previous pass),
`events.json` (recent activity), `paused_drives`.
Daemon log: `journalctl --user -u onedrive-rs`.

## Behaviour in short

- Online changes are read page by page from Microsoft Graph `/delta` and applied
  locally first, then local changes are uploaded (the D client's default order).
- Downloads go to `<file>.partial`, are hash-checked (QuickXorHash), then renamed;
  interrupted downloads resume.
- A file changed on both sides is never overwritten: the local version is kept as
  `<name>-<host>-safeBackup-0001.<ext>` and uploaded as a new file.
- Local moves and renames become online moves (no re-upload).
- Online deletions remove local files only if they are unchanged; local deletions
  of more than `classify_as_big_delete` items are refused.

## Development

```sh
cargo test
cargo run -p odrs-ipc --example mock_daemon   # fake daemon for GUI work
```

Crates: `quickxor`, `graph` (OAuth2, Graph client), `store` (SQLite), `engine`
(reconciliation), `ipc` (D-Bus), `watch` (inotify), `gui` (Slint), `app` (CLI).
