# Tunnels, groups, and everyday commands

[Back to README](../README.md)

## Create a tunnel

`--server` is required. It accepts a saved server name or an SSH config alias;
an alias is saved automatically with the new rule. Names are optional. Without
`--name`, fwm chooses an unused short word and prints it in the result.

```sh
# Local port 3000 reaches localhost:3000 on dev
fwm add --server dev --local --port 3000 --name web --wait

# Port 12222 on dev reaches localhost:22 on this machine
fwm add --server dev --remote --src 12222 --tgt 22 --name reverse-ssh

# Reach a database from the SSH server's network
fwm add --server dev --local=15432:db.internal:5432 --name database

# Save a rule without starting it
fwm add --server dev --local --port 8080 --name preview --disabled
```

Creation binds to `127.0.0.1` by default. A full mapping has the form
`[bind:]source_port:target_host:target_port`. Use brackets for IPv6, for example
`[::1]:3000:[::1]:3000`. Scoped targets such as `[fe80::1%lo0]:8080` are interpreted
on the target side. `--local` connects to the target from the SSH server;
`--remote` connects from the machine running fwm.

`add` saves and starts a rule. `--wait` waits up to 20 seconds for its listener;
`--timeout 500ms`, `20s`, or `2m` implies `--wait`. A timeout leaves the saved rule
in place and the daemon keeps trying. A ready listener does not prove that the
target application is healthy. `--disabled` cannot be combined with waiting.

Reverse rules use a dedicated SSH connection and verified cleanup by default.
If the server prohibits remote command execution, see
[reverse-tunnel recovery](operations.md#reverse-tunnel-recovery).

## SOCKS5

```sh
# Local SOCKS proxy; destinations are reached through dev
fwm add --server dev --dynamic 1080 --name socks

# SOCKS proxy on dev; destinations are reached through this machine
fwm add --server dev --remote-dynamic 127.0.0.1:7897 --name reverse-socks
```

A client on `dev` can use `socks5h://127.0.0.1:7897` for the second example; fwm
resolves and connects to its destination locally. No separate local SOCKS server
is needed. SOCKS supports unauthenticated CONNECT only, not UDP ASSOCIATE or
BIND. Keep listeners on loopback unless you intend to expose them; see the
[security notes](../SECURITY.md).

`--remote 7897` is a fixed forward to local `localhost:7897`, not a SOCKS proxy.
SOCKS options cannot be combined with fixed mappings or port-range shorthand.

## Port ranges and groups

```sh
# Forward each remote port to the same local port
fwm add --server dev --remote --port 3000-3003,8080 --name services

# Forward several local ports to a single remote target port
fwm add --server dev --local --src 4000-4003 --tgt 8080 --name pool

# Append members to an existing group
fwm add --server dev --remote --port 9000-9002 --group services --name extra

fwm group list
fwm down services
fwm up --group services --wait
```

Ranges include both ends. Duplicate ports are removed. A named batch creates a
group and names members with a source-port suffix, such as `services-3000`.
Unnamed batches get a random group name and separate random member names. Names
persist across restarts. `--group` chooses group membership independently of
`--name`.

Ports must be between 1 and 65535, with at most 512 rules in an instance. The whole
batch is validated and saved together; a configuration conflict rejects it.
Network failures are reported per rule. `--wait` uses one timeout for the batch.

Use either `--port` for matching source/target ports, or `--src` and `--tgt`
together when adding. `--tgt` accepts one port. You can also write
`--local 3000` or `--remote 3000-3003`, but cannot combine shorthand with a full
mapping.

## Edit and organize

```sh
fwm edit web --tgt 8080
fwm edit web --src 3001
fwm edit web --remote
fwm edit web --rename dashboard
fwm edit dashboard --server another-alias
fwm edit dashboard --group services
fwm edit dashboard --ungroup
fwm edit services --rename backend
fwm edit backend --group another-group
```

An edit changes only the fields you supply. `--src` keeps the binding address;
`--tgt` keeps the target host. `--port` changes both ports, preserving addresses.
Changing direction preserves the endpoints. Even a full replacement mapping
preserves the existing bind address if you omit it. Switching from SOCKS to a
fixed tunnel needs a target port, for example
`fwm edit reverse-socks --remote --tgt 8080`.

Renaming keeps the rule ID and does not reconnect. Changing endpoints closes
that rule's existing data connections. Moving to a new SSH alias saves the new
server and rule change together.

A group edit applies to all members in one transaction. It does not add ports;
use `add --group` to expand a group. Conflicting listener changes reject the
whole edit. To merge groups, use `edit GROUP --group TARGET`; renaming to an
existing group is rejected. `--ungroup` keeps the rules and removes membership.
Even stopped members of one group must have distinct listeners.

The older positional name for `add` still works. For numeric rule names, make
port arguments explicit: `fwm edit 1234 --remote --port 3000`.

## Stop, restart, retry, or remove

```sh
fwm down web
fwm up web --wait
fwm restart web --wait
fwm restart --server dev --wait
fwm retry --server dev
fwm down --all
fwm remove web
```

Rule names, IDs, and group names select individual rules or groups; commands
also offer `--group` and `--server` selectors. `restart` starts selected stopped
rules too. A rule/group restart leaves unselected rules running. A server
restart refreshes that server's SSH configuration and connection, useful after
changing keys, endpoints, or agents.

`retry` acts only on enabled rules in an abnormal state; it skips connected and
stopped rules. `down` saves a stopped setting; `remove` deletes the rule. Neither
starts a stopped daemon. Offline edits and `add --disabled` also keep it stopped.
For configuration drafts and reload behavior, see [configuration](configuration.md).
