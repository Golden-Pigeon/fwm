fwm v0.2.0 adds two-server relay forwarding and unifies target endpoint syntax.

- Relay rules listen on one server and forward each connection to a target
  resolved from a second server. The machine running fwm holds both SSH
  sessions, so the servers never talk to each other and all data flows through
  the manager: access to `localhost:8080` on A reaches `localhost:9000` on C.
- Both relay endpoints use rule-dedicated SSH sessions with verified cleanup on
  the source side by default, managed exactly like reverse rules. A refused
  target connection fails only that connection, never the sessions.
- Relay `--tgt` accepts `[HOST:]PORT`, the same shape as `--src`: a host in C's
  network is reachable with `--tgt db.internal:5432`, and C resolves the name.
  `--tgt-host` is removed; put the host in `--tgt` instead.

For example, bridge two servers through this machine:

```sh
fwm add --name a-to-c --src-server A --src 8080 --tgt-server C --tgt 9000
```

Saved configurations migrate from schema v3 to v4 when the daemon starts, and a
backup of the previous file is kept next to it. The IPC protocol moves to
version 2, so update the CLI and daemon together; the installers and packages
restart the default daemon, which performs the migration.

Prebuilt packages are available for macOS Intel/ARM64, Linux x86_64/ARM64 (musl),
and Windows x64. Each package includes license notices and the source of its
MPL-licensed dependency. `SHA256SUMS` covers every archive and the installer.

Install on macOS/Linux:

```sh
curl -fsSL https://github.com/Golden-Pigeon/fwm/releases/latest/download/install.sh | bash
```

The installer updates `~/.local/bin/fwm`, configures shell completion, and
starts/restarts the default daemon. Use `--no-start` to install without restarting.
For Windows, extract the zip and add the executable's directory to PATH.

Remote verified-cleanup helpers support Linux x86_64, macOS universal, and Windows
x86_64. This is separate from the local client architecture. For other remote
platforms or servers without command execution, explicitly use `--remote-cleanup off`.

See the installation guide and SECURITY.md for configuration and known dependency
limitations. Existing TCP sessions cannot be resumed across a disconnected SSH
connection; fwm restores listeners for new connections.
