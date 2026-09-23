# SSH keys, host trust, and compatibility

[Back to README](../README.md)

## Server profiles

Most tunnels can use an alias directly from `~/.ssh/config`. To give an alias a
separate fwm name or override connection settings:

```sh
fwm server add dev --ssh my-dev-server
fwm server add production --host example.com --user alice --port 22 --identity ~/.ssh/id_ed25519
fwm server edit production --port 2222 --user bob
fwm server edit production --rename prod
fwm server edit prod --unset user,port,identity
```

Edits preserve the server ID and change only supplied fields. `--unset` removes
an override so SSH config or defaults apply again. It accepts `user`, `port`,
`identity`, `ssh-config`, `known-hosts`, and `proxy-jump`, as a comma-separated
list or repeated option. `--proxy-jump none` explicitly disables jumps;
`--unset proxy-jump` restores inheritance. Clearing an identity override does
not disable authentication.

Use `--ssh-config PATH` with `add` or server commands for a separate SSH config.
CLI file paths are resolved relative to the current directory; paths written in
fwm's TOML are relative to its configuration directory. Existing final file
symlinks are preserved so you can replace their targets.

## Host trust and jump hosts

```sh
fwm server trust dev
fwm server trust dev --fingerprint SHA256:...
fwm server trust dev --hop 1
fwm server trust dev --hop bastion --fingerprint SHA256:...
```

Check fingerprints through a trusted channel. The daemon never accepts an
unknown key automatically. Noninteractive and JSON trust commands require a
fingerprint for a new key. Trusting an already trusted key succeeds without
another prompt, but any supplied fingerprint must still match. Changed or
revoked keys remain blocked.

For ProxyJump, trust hops in order, then trust the target. Hop numbers start at
1; a unique alias also works. `server trust TARGET --hop HOP` uses the hop's
configuration and known-hosts file as seen through that target's jump chain.

`trust`, `server check`, and `doctor --server` accept unregistered SSH aliases.
They do not create server records or start a stopped daemon. When the daemon is
running, successful trust automatically retries enabled rules blocked by that
host's trust problem.

## Keys and agents

fwm supports private keys, user certificates, and unlocked SSH agents on Unix
and Windows. Add an encrypted key to your system agent with `ssh-add` before
using it. `IdentityFile key.pub` can select the matching key from the agent,
including with `IdentitiesOnly yes`. Missing or invalid explicitly selected
identity files are reported with their paths. RSA authentication uses negotiated
SHA-256/512 signatures.

If an agent refuses a signature, unlock or authorize it and retry the rule.
Diagnostics show the process/PID and agent socket actually used. With a running
daemon, checks and trust run in that daemon's environment; offline, they use the
CLI environment.

If your terminal has a new agent socket but the daemon still has the old one,
set `IdentityAgent` in SSH config and run `fwm restart --server dev`. For a
standalone daemon, `fwm daemon restart` from the new environment is another
option. A login service gets its environment from the OS service manager and may
not inherit the current terminal's agent.

## SSH config compatibility

Supported settings include `Host`, `Include`, `HostName`, `User`, `Port`,
`IdentityFile`, `IdentityAgent`, `IdentitiesOnly`, known-hosts paths, and native
ProxyJump. Host verification is always enabled, including hashed known-hosts,
nondefault ports, changed keys, and revocations.

Relative identity, agent, and known-hosts paths use the directory containing the
SSH file that declares them, including included files. Relative `Include` paths
use `~/.ssh`. `~` and `%d` refer to the running user's home directory.

`CanonicalizeHostname no/yes/always` (also `false/true`) supports IP
normalization and lowercase hostname rematching without DNS rewriting. Advanced
rules such as `CanonicalDomains` and `CanonicalizePermittedCNAMEs`, trailing-dot
names needing DNS handling, and nonstandard numeric addresses are rejected.

Complex `Match`, arbitrary `ProxyCommand`, host-certificate trust, and
`UseKeychain yes` are not implemented and produce errors. Use a reduced SSH
config through `--ssh-config` when necessary. The server must permit forwarding;
remote binding is also subject to its `GatewayPorts` policy.
