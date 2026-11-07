fwm v0.2.3 fixes macOS managed-service restarts during installation and upgrades.

- Wait for launchd to finish removing the old service before registering its
  replacement. A daemon releasing its instance lock does not mean its launchd
  job has disappeared yet; restarting too early could fail with `No such process`
  or `Bootstrap failed: 5: Input/output error`.
- Accept `bootout`'s `No such process` result only when a subsequent service query
  confirms the job is absent. Permission failures, query errors, and unconfirmed
  removal still fail explicitly and retain the existing rollback behavior.
- The fix applies to both installers and direct `fwm daemon restart` commands.
  The automatic SSH recovery improvements from v0.2.2 are included.

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
