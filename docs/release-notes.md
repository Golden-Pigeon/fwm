fwm v0.2.2 improves automatic recovery when an SSH connection is interrupted.

- Server timeouts and disconnects during authentication now retry automatically,
  including through ProxyJump, instead of incorrectly stopping at `needs attention`.
  Local SSH agent stalls, refused signatures, explicit authentication rejection,
  and trust/configuration errors still require attention.
- Recovery-helper channels that close without an exit result retry with backoff.
  Such a closure is inconclusive: a persistent server problem may also keep
  retrying, with the latest error visible in status.
- Confirmed unexpected helper exit statuses and signals still require attention.
  Exit results and stderr are preserved after execution acknowledgement and EOF;
  startup or reply timeouts no longer hide a confirmed execution failure.
- Established reverse and relay listeners probe the SSH request path to detect
  a stalled connection. Unconfirmed listener cancellation is bounded during
  teardown so recovery can proceed while retaining the cancellation warning.
- Bash completion omits saved candidates containing `$` or backticks to avoid
  Bash 5.3 interpreting substitution syntax while quoting them. Such values
  remain usable as manually single-quoted arguments or through safe names/IDs.

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
