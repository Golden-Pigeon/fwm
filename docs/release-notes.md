The first public release of fwm, a persistent SSH port-forwarding manager.

- Named local and reverse tunnels, port ranges, and groups.
- SOCKS5 proxies in either direction, SSH aliases, jump hosts, and host-key verification.
- Automatic reconnection and verified recovery of stale reverse-tunnel listeners.
- Compact status output, logs, JSON output, and user login services.

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
