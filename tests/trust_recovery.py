"""Trust a failed forward and resume it without touching healthy/stopped rules."""
import asyncio
import json
from port_shorthand import free_block


async def check_trust_recovery(cli, rpc, eventually, root, ssh_port, ssh_user,
                               key, fingerprint, target_port, healthy_port, binary, direct_ssh_port):
    await check_offline_trust(binary, root, ssh_port, ssh_user, key, fingerprint)
    await check_contextual_hop_trust(cli, root, ssh_port, direct_ssh_port, ssh_user, key, fingerprint)
    known_hosts = root / "trust-recovery-known-hosts"
    direct_config = root / "trust-recovery-direct-ssh-config"
    direct_config.write_text("Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n")
    start = free_block(3)
    names = ["trust-pending", "trust-stopped", "trust-second"]

    async def states():
        return {rule["name"]: rule for rule in (await rpc("status"))["data"]["forwards"]}

    async def state_is(name, wanted):
        return (await states()).get(name, {}).get("state") == wanted

    async def ping(reader, writer, payload):
        writer.write(payload)
        await writer.drain()
        assert await asyncio.wait_for(reader.readexactly(len(payload)), 5) == payload

    healthy_reader, healthy_writer = await asyncio.open_connection("127.0.0.1", healthy_port)
    resumed_writer = None
    try:
        await cli("server", "add", "trust-fixture", "--host", "127.0.0.1", "--port", str(ssh_port),
                  "--user", ssh_user, "--identity", str(key), "--known-hosts", str(known_hosts),
                  "--ssh-config", str(direct_config))
        code, out, error = await cli("--json", "add", names[0], "--server", "trust-fixture",
                                     "--local", "--src", str(start), "--tgt", str(target_port),
                                     "--connection-mode", "dedicated", "--wait", "--timeout", "10s", success=False)
        assert code == 3 and not out and json.loads(error)["error"]["code"] == "needs_attention"
        await cli("add", names[1], "--server", "trust-fixture", "--local", "--src", str(start + 1),
                  "--tgt", str(target_port), "--disabled")
        code, out, error = await cli("--json", "server", "trust", "trust-fixture",
                                     "--fingerprint", "SHA256:incorrect", success=False)
        assert code != 0 and not out and "mismatch" in error
        assert not known_hosts.exists()
        assert (await states())[names[0]]["state"] == "needs_attention"

        _, out, _ = await cli("--json", "server", "trust", "trust-fixture", "--fingerprint", fingerprint)
        assert json.loads(out)["data"]["status"] == "trusted"
        await eventually(lambda: state_is(names[0], "established"), description="trust resumes blocked forward")
        assert (await states())[names[1]]["desired_state"] == "stopped"
        assert (await states())[names[1]]["state"] == "stopped"
        resumed_reader, resumed_writer = await asyncio.open_connection("127.0.0.1", start)
        await ping(resumed_reader, resumed_writer, b"recovered-after-trust")

        # Keep an established session for this very profile while a new,
        # dedicated session observes a newly missing trust file.
        known_hosts.unlink()
        code, out, error = await cli("--json", "add", names[2], "--server", "trust-fixture",
                                     "--local", "--src", str(start + 2), "--tgt", str(target_port),
                                     "--connection-mode", "dedicated", "--wait", "--timeout", "10s", success=False)
        assert code == 3 and not out and json.loads(error)["error"]["code"] == "needs_attention"
        ssh_config = root / "trust-target-alias-config"
        ssh_config.write_text(f"""Host trust-target-alias
 HostName 127.0.0.1
 Port {ssh_port}
 User {ssh_user}
 IdentityFile {key}
 UserKnownHostsFile {known_hosts}
 GlobalKnownHostsFile none
""")
        await cli("server", "trust", "trust-target-alias", "--ssh-config", str(ssh_config),
                  "--fingerprint", fingerprint)
        await eventually(lambda: state_is(names[2], "established"), description="trust resumes only blocked session")
        assert "trust-target-alias" not in {server["name"] for server in (await rpc("get_config"))["data"]["servers"]}
        await ping(resumed_reader, resumed_writer, b"healthy-same-server-survives")
        await ping(healthy_reader, healthy_writer, b"healthy-other-server-survives")
        assert (await states())[names[1]]["state"] == "stopped"

        # Trusting an already trusted server must also preserve its streams.
        _, out, _ = await cli("--json", "server", "trust", "trust-fixture")
        assert json.loads(out)["data"]["status"] == "trusted"
        await ping(resumed_reader, resumed_writer, b"repeated-trust-survives")

        # Server-scoped history must contain the trust and transport events,
        # not only the individual forward's state transitions.
        server_id = next(server["id"] for server in (await rpc("get_config"))["data"]["servers"]
                         if server["name"] == "trust-fixture")

        async def server_history_complete():
            _, output, _ = await cli("--json", "logs", "--server", "trust-fixture", "--tail", "200")
            events = json.loads(output)["events"]
            assert events and all(entry["server_id"] == server_id for entry in events)
            scoped = [entry["event"]["message"] for entry in events if not entry["event"]["forward_id"]]
            return (any("host key explicitly trusted" in message for message in scoped)
                    and "SSH connection established" in scoped)

        await eventually(server_history_complete, description="server trust and connection history attribution")
        print("PASS trust resumes blocked forwards, preserves stopped/healthy rules, rejects wrong fingerprints", flush=True)
    finally:
        if resumed_writer:
            resumed_writer.close()
        healthy_writer.close()
        await cli("remove", "--server", "trust-fixture", success=False)
        await cli("server", "remove", "trust-fixture", success=False)


async def check_offline_trust(binary, root, ssh_port, ssh_user, key, fingerprint):
    config_dir = root / "offline-trust-manager"
    ssh_config = root / "offline-trust-ssh-config"
    known_hosts = root / "offline-trust-known-hosts"
    # A blocked event journal must not turn a successful trust mutation into
    # failure, or make the user believe that the key was not persisted.
    (config_dir / "state" / "events.jsonl").mkdir(parents=True)
    ssh_config.write_text(f"""Host offline-alias
 HostName 127.0.0.1
 Port {ssh_port}
 User {ssh_user}
 IdentityFile {key}
 IdentitiesOnly yes
 IdentityAgent none
 UserKnownHostsFile {known_hosts}
 GlobalKnownHostsFile none
""")

    async def offline_cli(*args):
        process = await asyncio.create_subprocess_exec(
            str(binary), "--config-dir", str(config_dir), "--json", *args,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        out, error = await asyncio.wait_for(process.communicate(), 30)
        assert process.returncode == 0, (args, out, error)
        return json.loads(out)

    try:
        trusted = await offline_cli("server", "trust", "offline-alias", "--ssh-config", str(ssh_config),
                                    "--fingerprint", fingerprint)
        assert trusted["data"]["status"] == "trusted"
        assert trusted["data"]["trusted"] is True
        assert trusted["data"]["warnings"] and "history" in trusted["data"]["warnings"][0]
        repeated = await offline_cli("server", "trust", "offline-alias", "--ssh-config", str(ssh_config))
        assert repeated["data"]["status"] == "trusted"
        assert fingerprint and known_hosts.exists()
        assert not (await offline_cli("daemon", "status"))["daemon_running"]
        assert (await offline_cli("server", "list"))["servers"] == []
        await offline_cli("server", "check", "offline-alias", "--ssh-config", str(ssh_config))
        await offline_cli("doctor", "--server", "offline-alias", "--ssh-config", str(ssh_config))
        assert not (await offline_cli("daemon", "status"))["daemon_running"]
        assert (await offline_cli("server", "list"))["servers"] == []
    finally:
        await offline_cli("daemon", "stop")


async def check_contextual_hop_trust(cli, root, jump_port, target_port, user, key, fingerprint):
    ssh_config = root / "contextual-hop-config"
    known_hosts = root / "contextual-hop-known-hosts"
    unrelated_database = root / "jump-alias-own-known-hosts"
    ssh_config.write_text(f"""Host context-jump
 HostName 127.0.0.1
 Port {jump_port}
 User {user}
 IdentityFile {key}
 UserKnownHostsFile {unrelated_database}
Host context-target
 HostName 127.0.0.1
 Port {target_port}
 User {user}
 IdentityFile {key}
 ProxyJump context-jump
Host *
 IdentityAgent none
 GlobalKnownHostsFile none
""")
    await cli("server", "add", "context-target", "--ssh", "context-target", "--ssh-config", str(ssh_config),
              "--known-hosts", str(known_hosts))
    try:
        code, out, error = await cli("--json", "server", "trust", "context-target", success=False)
        assert code != 0 and not out and "--hop" in error and str(known_hosts.resolve()) in error
        code, out, error = await cli("--json", "server", "trust", "context-target", "--hop", "context-jump",
                                     "--fingerprint", "SHA256:incorrect", success=False)
        assert code != 0 and not known_hosts.exists()
        _, out, _ = await cli("--json", "server", "trust", "context-target", "--hop", "context-jump",
                              "--fingerprint", fingerprint)
        assert json.loads(out)["data"]["known_hosts"] == str(known_hosts.resolve())
        assert not unrelated_database.exists()
        assert f"[127.0.0.1]:{jump_port} " in known_hosts.read_text()
        assert f"[127.0.0.1]:{target_port} " not in known_hosts.read_text()
        await cli("server", "trust", "context-target", "--fingerprint", fingerprint)
        await cli("server", "check", "context-target")
        await cli("--json", "server", "trust", "context-target", "--hop", "1")
        print("PASS contextual hop trust uses the target database, remains explicit, and is idempotent", flush=True)
    finally:
        await cli("server", "remove", "context-target", success=False)
