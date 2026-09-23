# fwm

A command-line manager for SSH port forwarding. Save tunnels by name, manage them
across servers, and let a background daemon reconnect when the network drops.
Closing your terminal leaves the tunnels running.

Supports local and reverse forwarding, SOCKS5 in either direction, SSH config
aliases, and jump hosts on macOS, Linux, and Windows. The remote host needs an SSH
server; you do not need to install fwm there.

## Install

From a source checkout, with Rust 1.90+ and a C compiler:

```sh
./install-from-source.sh     # macOS / Linux
```

This installs to `~/.local/bin`, sets up Bash/Zsh completion, and starts or
restarts the default daemon. Open a new terminal afterward. On Windows, use
`cargo install --path crates/fwm --locked --force`.
[Installation options](docs/installation.md) cover custom paths and manual builds.

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
