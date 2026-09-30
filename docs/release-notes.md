fwm v0.1.1 improves source-address syntax and SSH configuration compatibility.

- `--src` now accepts an IP address with a port, range, or list, such as
  `--src 0.0.0.0:23589 --tgt 22`. Bracket IPv6 addresses. Editing with an explicit
  IP updates the binding; a port-only edit preserves the existing IP.
- SSH configurations using `PreferredAuthentications publickey` now work.
  Method lists containing `publickey` are accepted; lists excluding it report
  that fwm requires noninteractive public-key authentication.

For example, expose the local SSH service through a remote server:

```sh
fwm add --server dev --remote --src 0.0.0.0:23589 --tgt 22 --name reverse-ssh
```

The server must allow the requested binding, for example with
`GatewayPorts clientspecified`. Existing saved rules and configuration files
require no migration.

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
