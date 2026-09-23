# Configuration, backups, and recovery

[Back to README](../README.md)

## Configuration directories

The default is `~/.fwm/config.toml` on macOS/Linux and
`%USERPROFILE%\.fwm\config.toml` on Windows. Use `--config-dir PATH` for a separate
instance:

```sh
fwm --config-dir ~/work-fwm status
```

Installation paths do not change the configuration location. Equivalent paths
are normalized to the same daemon and login service identity. The configuration
directory itself and its `state` subdirectory cannot be final symlinks.

| File under the configuration directory | Purpose |
| --- | --- |
| `config.toml` | Editable candidate configuration. |
| `state/applied.toml` | Last committed configuration. |
| `state/applied.vN.toml` | Backup made when upgrading an older format. |
| `state/recovery.json` | Persistent manager identity and recovery generations. |
| `state/recovery-backups/` | Backups from explicit configuration recovery. |
| `state/daemon.lock` | Prevents multiple daemons for one instance. |
| `state/daemon.sock` | Unix control socket; long paths use a private short-path fallback. Windows uses a named pipe. |
| `state/events.jsonl` | Bounded event log with one rotated file. |
| `state/daemon.log` | Startup/runtime diagnostics, up to 2 MiB plus one rotated file. `RUST_LOG` controls verbosity. |

Older installations in platform-specific configuration directories are not
moved automatically. You can keep using one with `--config-dir`. To migrate,
stop its daemon, uninstall its login service if present, and move the whole
directory including `state`. Then install or start the instance at the new path.

## Manual edits

```sh
fwm config export
fwm config validate
fwm config reload
```

Edit `config.toml`, validate it, then reload. An invalid or missing candidate does
not erase committed rules; a restarted daemon uses the last valid snapshot.
With a running daemon, reload also rereads SSH connection settings and refreshes
servers whose resolved settings changed. Use `restart --server` to force a new
connection after changing a key file's contents.

While you have unapplied edits, ordinary additions and edits refuse to overwrite
the draft. `down` and `remove` can still save stop/delete decisions without
replacing it. Reloading the draft later respects those decisions.

Unknown server/rule fields are rejected, so misspellings such as `usr` or
`desired_sate` produce validation errors. Omitted IDs are derived from object
kind and name on first load, then saved by a successful edit. Existing explicit
IDs and IDs of renamed objects are preserved. If a name conflicts with another
object's ID, fix the name rather than changing IDs. Group names cannot collide
with rule names or IDs.

Reverse rules with no `remote_cleanup` field default to verified cleanup and a
dedicated connection. An explicit `remote_cleanup = "off"` is preserved.
For reverse SOCKS, use `kind = "remote_dynamic"` and a `listen` address without a
`target`.

The current configuration format is version 3. Upgrades retain old snapshots;
version 1 reverse rules migrate to verified/dedicated mode. Older multi-rule
batches named `prefix-port` become groups where that prefix does not conflict
with a rule name or ID. Rule IDs, endpoints, and requested states are preserved.

Limits are 128 server profiles, 512 rules, and 256 KiB of serialized
configuration. IDs are 1–128 bytes. IPv4/IPv6 listener overlaps, including mapped
addresses and wildcard binds, are validated before a change is saved.

## Recover a damaged applied snapshot

If `state/applied.toml` is damaged but you have repaired `config.toml`:

```sh
fwm daemon stop
fwm config validate
fwm config recover --from-candidate
fwm status
fwm up --server dev --wait     # enable the rules you want to run again
```

Recovery requires a stopped daemon and an exclusive instance lock. It validates
the candidate and backs up both files under `state/recovery-backups/<uuid>/`.
Readable stop/delete decisions are retained, IDs are preserved, and all remaining
rules are saved as stopped. Repeated recovery creates separate backups.

Only if the control-intent record cannot be read, the command asks for
`--discard-unreadable-intent` to discard that record explicitly. Recovered rules
still remain stopped until you enable them.

For permission errors or an untrusted control socket, consult the
[security notes](../SECURITY.md); fwm does not treat an authentication failure as
permission to fall back to an offline edit.
