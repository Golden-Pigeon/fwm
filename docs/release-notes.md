fwm v0.2.1 fixes upgrades over a machine whose v0.1.x daemon is still running.

- Installing v0.2.0 on such a machine could fail: the new client could not
  stop the older daemon, `install.sh` reported that the restart failed, and
  the suggested retry could not succeed either. `fwm daemon stop` and
  `daemon restart` now retire a daemon one protocol generation behind over its
  unchanged Shutdown request, so upgrades replace it gracefully while keeping
  saved rules and their running or stopped intent.
- A daemon reporting a newer, unknown protocol version is never replaced or
  killed automatically; the error names that version and asks to upgrade fwm
  first. All commands other than this one shutdown bridge keep strict
  protocol negotiation with its actionable upgrade guidance.
- Two-server relay forwarding and the unified `--tgt [HOST:]PORT` syntax from
  v0.2.0 are unchanged.

If a v0.2.0 installation already printed the restart failure, end the old
daemon process once and start the replacement:

```sh
pgrep -af fwm                # find the old daemon process
kill -TERM <its PID>
fwm daemon restart
```

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
