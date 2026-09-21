"""Force a real stale sshd listener, then verify fenced automatic recovery."""
import asyncio
import contextlib
import json
import os
import socket
import signal
import tempfile
from pathlib import Path


def records(directory):
    result = []
    for path in directory.glob("leases/*/*.json"):
        try:
            value = json.loads(path.read_text())
            if value.get("phase") == "confirmed":
                result.append((path, value))
        except (OSError, ValueError):
            pass
    return result


def record_for(directory, port):
    return next(((path, value) for path, value in records(directory) if value["listen_port"] == port), None)


async def check_remote_recovery(cli, rpc, echo_test, status, eventually, directory,
                                held_peers, remote_port, local_port, sshd, config_dir,
                                binary, server_port, proxy_port, user, identity, known_hosts, target_port,
                                allow_new_connections):
    entry = record_for(directory, remote_port)
    assert entry, "remote listener must have a confirmed identity lease"
    _, previous = entry
    old_pid = previous["session"]["pid"]
    peer = previous["transport"][1]
    stable_reader, stable_writer = await asyncio.open_connection("127.0.0.1", local_port)

    async def stable_ping():
        stable_writer.write(b"unrelated-connection-survives")
        await stable_writer.drain()
        assert await asyncio.wait_for(stable_reader.readexactly(29), 4) == b"unrelated-connection-survives"

    # Another manager using the same server/port must not reclaim our lease.
    with tempfile.TemporaryDirectory(prefix="fwm-other-owner-") as other:
        direct_config = Path(other) / "direct-ssh-config"
        direct_config.write_text("Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n")
        async def other_cli(*args, success=True):
            process = await asyncio.create_subprocess_exec(str(binary), "--config-dir", other, *args,
                                                          stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, stderr = await asyncio.wait_for(process.communicate(), 30)
            if success:
                assert process.returncode == 0, (args, stderr.decode(), stdout.decode())
            return process.returncode

        try:
            await other_cli("server", "add", "test", "--host", "127.0.0.1", "--port", str(proxy_port),
                            "--user", user, "--identity", str(identity), "--known-hosts", str(known_hosts),
                            "--ssh-config", str(direct_config))
            code = await other_cli("add", "foreign", "--server", "test", "--remote", "--src", str(remote_port),
                                   "--tgt", str(target_port), "--wait", "--timeout", "8s", success=False)
            assert code != 0
            os.kill(old_pid, 0)
            await echo_test(remote_port)
        finally:
            with contextlib.suppress(Exception):
                await other_cli("daemon", "stop", success=False)

    allow_new_connections.clear()
    held_peers.add(peer)
    try:
        async def lost():
            current = (await status())["remote"]
            return current["state"] != "established"

        await eventually(lost, timeout=35, description="verified remote SSH blackhole detection")
        os.kill(old_pid, 0)
        assert sshd.poll() is None
        # The fault proxy retains the server socket after the client's timeout.
        # Therefore a successful replacement necessarily needs active cleanup.
        with socket.socket() as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                probe.bind(("127.0.0.1", remote_port))
            except OSError:
                pass
            else:
                raise AssertionError("test did not preserve the stale remote listener")
        await stable_ping()
        allow_new_connections.set()

        async def reclaimed():
            current = record_for(directory, remote_port)
            return (current and current[1]["generation"] > previous["generation"]
                    and current[1]["session"]["pid"] != old_pid
                    and (await status())["remote"]["state"] == "established")

        await eventually(reclaimed, timeout=30, description="verified cleanup and remote rebind")
        with contextlib.suppress(ProcessLookupError):
            os.kill(old_pid, 0)
            raise AssertionError("old SSH session still exists after verified recovery")
        assert sshd.poll() is None, "must never stop the sshd master"
        await echo_test(remote_port)
        await stable_ping()
        print("PASS real stale sshd actively terminated/rebound; foreign manager and sibling TCP protected", flush=True)
    finally:
        allow_new_connections.set()
        held_peers.discard(peer)
        stable_writer.close()

    # Down/up must release only its own generation, leave no managed listener,
    # and use a fresh SSH connection rather than attempting to claim itself.
    await cli("down", "remote")

    async def released():
        return record_for(directory, remote_port) is None

    await eventually(released, description="normal stop releases remote lease")
    await cli("up", "remote", "--wait", "--timeout", "15s")
    await echo_test(remote_port)
    print("PASS normal stop releases ownership and up establishes a new verified session", flush=True)

    # A helper crash must be noticed while the transport is still healthy, not
    # turn the next network interruption into an unrecoverable stale lease.
    _, current = record_for(directory, remote_port)
    os.kill(current["helper"]["pid"], signal.SIGKILL)

    async def helper_replaced():
        replacement = record_for(directory, remote_port)
        return (replacement and replacement[1]["generation"] > current["generation"]
                and (await status())["remote"]["state"] == "established")

    await eventually(helper_replaced, timeout=30, description="helper crash recovery")
    await echo_test(remote_port)
    print("PASS unexpected helper death causes supervised session recovery", flush=True)

    async def unrelated(reader, writer):
        writer.write(b"unrelated")
        await writer.drain()
        writer.close()

    blocker = await asyncio.start_server(unrelated, "127.0.0.1", 0)
    busy_port = blocker.sockets[0].getsockname()[1]
    try:
        code, _, _ = await cli("add", "busy-port", "--server", "test", "--remote", "--src", str(busy_port),
                               "--tgt", str(target_port), "--wait", "--timeout", "2s", success=False)
        assert code != 0
        reader, writer = await asyncio.open_connection("127.0.0.1", busy_port)
        assert await reader.read() == b"unrelated"
        writer.close()
    finally:
        blocker.close()
        await blocker.wait_closed()

    async def busy_port_recovered():
        return (await status())["busy-port"]["state"] == "established"

    await eventually(busy_port_recovered, timeout=35, description="unmanaged port release and automatic retry")
    await echo_test(busy_port)
    await cli("remove", "busy-port")
    print("PASS unrelated listener protected; automatic retry succeeds after its port is released", flush=True)
