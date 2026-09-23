# fwm

A command-line manager for SSH port forwarding. Save tunnels by name, manage them
across servers, and let a background daemon reconnect when the network drops.
Closing your terminal leaves the tunnels running.

Supports local and reverse forwarding, SOCKS5 in either direction, SSH config
aliases, and jump hosts on macOS, Linux, and Windows. The remote host needs an SSH
server; you do not need to install fwm there.

## Install

On macOS or Linux, download and install the latest prebuilt release:

```sh
curl -fsSL https://github.com/Golden-Pigeon/fwm/releases/latest/download/install.sh | bash
```

The installer verifies SHA-256 checksums, installs to `~/.local/bin`, enables
Bash/Zsh completion, and starts or restarts the default daemon. Open a new
terminal afterward. No Rust toolchain is needed.

On Windows, extract the x64 zip from [Releases](https://github.com/Golden-Pigeon/fwm/releases)
and add its directory to PATH. For a specific version, a custom install path,
or a source build, see [installation options](docs/installation.md).

## Quick start

Replace `dev` with a host alias from your SSH config. Check the host fingerprint
before trusting it, then create a tunnel:

```sh
fwm server trust dev
fwm add --server dev --local --port 3000 --name web --wait
fwm status
fwm down web                 # stop; keep the saved rule
fwm up web                   # start it again
```

This forwards local port 3000 to `localhost:3000` on `dev`. Use `--remote` for the
opposite direction, or `--dynamic 1080` for a local SOCKS5 proxy. To start saved
running tunnels at login, run `fwm service install`.

## Documentation

- [Installation and shell completion](docs/installation.md)
- [Tunnels, groups, and everyday commands](docs/usage.md)
- [SSH keys, host trust, and compatibility](docs/ssh.md)
- [Status, logs, services, and troubleshooting](docs/operations.md)
- [Configuration, backups, and recovery](docs/configuration.md)

[MIT license](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md) · [Security notes](SECURITY.md)
