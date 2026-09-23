# Status, logs, services, and troubleshooting

[Back to README](../README.md)

## Read status and logs

```sh
fwm status
fwm status --watch
fwm status web
fwm status --server dev
fwm status --group services
fwm --json status
fwm logs web --follow
fwm logs --server dev --tail 50
fwm logs --group services
fwm doctor --server dev
```

Status shows one row per rule: name, server, group, direction, source, target,
and state. `L→R` means a local listener with a remote target; `R→L` is the reverse.
`localhost`, `127.0.0.1`, and `::1` show only the port. Other addresses, including
`0.0.0.0`, keep the host and port; SOCKS targets display `SOCKS5`.

The summary counts states, problems appear first, and error details follow the
table. Colors are optional: set `NO_COLOR=1` to disable them. Redirected output
has no colors. JSON retains full addresses and the original state names:

| Display | JSON state | Meaning |
| --- | --- | --- |
| `connected` | `established` | SSH authenticated and listener established; target health is not checked. |
| `retry in Ns` / `retrying` | `backoff` | Waiting to reconnect or recreate a listener. |
| `starting` | `starting` | Establishing a connection or listener. |
| `needs attention` | `needs_attention` | Trust, authentication, or configuration needs fixing. |
| `stopping` | `stopping` | Shutdown or remote cancellation is in progress. |
| `unverified` | `unverified` | The runtime or a protocol result cannot yet be confirmed. |
| `stopped` | `stopped` | The rule is stopped while the daemon is running. |

An offline daemon has its own notice. Saved running settings are not evidence
that a port is listening. During an outage, `status --watch` continues waiting
for recovery. JSON can report `daemon_running: null` with
`runtime_available: false`. Use Ctrl-C to stop a watch or log follow.

Logs default to the last 100 events and show UTC dates. `--tail 0 --follow` shows
only new events. Retained history remains readable after stops, restarts, and
deletions. Rule/server selections follow stable IDs through renames; current
names take priority over old aliases, so use an ID to select an old object
unambiguously. Group logs use the group label at the time of each event; query
an old group's name for its earlier records.

A status watch follows a selected rule/server through renames. Group watches
include new members but need a new query after a group rename. An empty selection
after deletion keeps watching. Large status responses may shorten error details;
use `fwm status RULE_ID` for the full diagnostic.

One-shot `--json` commands emit one final object. A failed wait can return
`ok:false` with `data.saved:true`: the rule was saved but did not become ready.
Watch/follow emit JSON Lines.

## Daemon and login services

Enabling a tunnel starts the current user's daemon as needed. Closing a terminal
does not stop it. Queries, SSH diagnostics, and offline configuration edits do
not start it.

```sh
fwm daemon start
fwm daemon status
fwm daemon stop
fwm daemon restart
fwm daemon run                 # foreground process for debugging or supervision
fwm service install            # start now and at user login
fwm service status
fwm service uninstall
```

Login services use a macOS LaunchAgent, a Linux systemd user service, or a
Windows login task for the current user. `--user` is accepted for compatibility;
system-wide services before login are not configured. Credentials must be
available in the service's environment. Keys needing interactive unlocking can
leave rules in `needs attention`.

After upgrading or moving the executable, run `fwm daemon restart`. It preserves
saved running/stopped settings and refreshes managed service paths, keeping the
existing login-enabled setting. It starts the daemon if it was stopped.

`service uninstall` stops this instance's daemon and tunnels, then removes its
login registration. It preserves saved rules and their requested states. They
can resume on a later daemon start. `service status` distinguishes a definition
file from a registered, running, or enabled service.

If restart cannot confirm that the old process stopped, it fails rather than
starting a second instance. A service error includes the failed stage; check
`service status` and `daemon status` before retrying. If recovery of a prior
service definition is unavailable, the error explains when uninstalling and
reinstalling is required.

## Connection recovery

After a disconnect, fwm retries quickly, then uses exponential backoff with
jitter up to 30 seconds. Defaults are `keepalive_interval_secs = 5` and
`keepalive_max = 3`; a silent network black hole may take about 20 seconds to
detect. An explicit TCP reset is detected sooner. These values are configurable
in TOML.

Reconnection restores listeners for new TCP connections. It cannot resume an
application connection that was already broken. Failure to reach one target
application affects that data connection; loss of a shared SSH connection affects
all rules using it. Local and local SOCKS rules normally share a server
connection; reverse rules normally have dedicated connections.

For trust or authentication errors, fix the cause and run
`fwm retry --server dev`. For changed SSH endpoints, key files, or agent settings,
use `fwm restart --server dev` to force a fresh connection.

## Reverse-tunnel recovery

Reverse forwarding uses verified cleanup by default. fwm uploads and runs a
bundled native helper on the remote host to identify its own old SSH session and
release a stale listener before reconnecting. It needs no remote Python,
compiler, package manager, root access, or permanent service. The temporary
executable is removed after launch.

Bundled helpers cover Linux x86_64, macOS universal, and Windows x86_64. Other
platforms or restricted servers can report an unsupported or attention state.
The macOS helper's full remote-SSH workflow still needs native acceptance testing;
see the [testing notes](../TESTING.md) for recorded platform coverage.
For a server that cannot run the helper, explicitly disable cleanup:

```sh
fwm add --server restricted --remote --port 8080 --remote-cleanup off
fwm edit existing-rule --remote-cleanup verified
```

With cleanup off, fwm waits for the SSH server to release an old listener.
Verified cleanup only terminates a registered old session belonging to the same
manager and rule after checking ownership again. It does not kill an unrelated
process occupying the port, even if that process belongs to your SSH user.
`unmanaged_conflict` means that ownership could not be verified or the listener
is external; inspect the conflict or wait for its owner to release it.

Remote registrations live under `$XDG_STATE_HOME/fwm/leases` or
`~/.local/state/fwm/leases`. Keep these and local `state/recovery.json` while
forwards are running: they provide the evidence needed to recognize old sessions.
