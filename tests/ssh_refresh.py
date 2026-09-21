"""Audit U18 through CLI/RPC while another server profile carries live traffic."""
import asyncio
import json
import socket
import time


async def check_ssh_refresh(cli, rpc, echo_test, root, ssh_port, ssh_user, key, known_hosts, echo_port, sibling_port):
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        listen_port = reservation.getsockname()[1]
    config = root / "refresh-ssh-config"
    original = f"""Host refresh-ssh
 HostName 127.0.0.1
 Port {ssh_port}
 User {ssh_user}
 IdentityFile {key}
 IdentitiesOnly yes
 IdentityAgent none
 UserKnownHostsFile {known_hosts}
 GlobalKnownHostsFile none
"""
    config.write_text(original)
    sibling_reader, sibling_writer = await asyncio.open_connection("127.0.0.1", sibling_port)

    async def sibling_alive(payload):
        sibling_writer.write(payload)
        await sibling_writer.drain()
        assert await asyncio.wait_for(sibling_reader.readexactly(len(payload)), 3) == payload

    async def state():
        reply = await rpc("status")
        return next(rule["state"] for rule in reply["data"]["forwards"] if rule["name"] == "refresh-rule")

    async def await_state(expected):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            observed = await state()
            if observed in expected:
                return
            await asyncio.sleep(.1)
        raise AssertionError(f"refresh-rule did not reach {expected}; observed {observed}")

    try:
        await cli("add", "refresh-rule", "--server", "refresh-ssh", "--ssh-config", str(config),
                  "--local", f"{listen_port}:127.0.0.1:{echo_port}", "--wait")
        await echo_test(listen_port)
        config.write_text(original.replace(f" Port {ssh_port}\n", " Port 1\n"))
        code, _, _ = await cli("server", "check", "refresh-ssh", success=False)
        assert code != 0
        code, out, error = await cli("--json", "restart", "--server", "refresh-ssh", "--timeout", "700ms", success=False)
        assert code != 0 and not out, (code, out, error)
        failure = json.loads(error)
        assert failure["data"]["saved"] and failure["data"]["ready"] is False
        assert await state() != "established", "server restart kept the stale authenticated SSH session"
        await sibling_alive(b"after-server-refresh")
        config.write_text(original)
        await cli("config", "reload")
        await await_state({"established"})
        await echo_test(listen_port)
        config.write_text(original.replace(f" Port {ssh_port}\n", " Port 1\n"))
        await cli("config", "reload")
        await await_state({"backoff", "needs_attention"})
        await sibling_alive(b"after-config-refresh")
        config.write_text(original)
        await cli("config", "reload")
        await await_state({"established"})
        await echo_test(listen_port)
        await sibling_alive(b"after-refresh-recovery")
        print("PASS server restart and config reload re-resolve SSH aliases while other server streams survive", flush=True)
    finally:
        sibling_writer.close()
        await sibling_writer.wait_closed()
        await cli("remove", "refresh-rule", success=False)
        await cli("server", "remove", "refresh-ssh", success=False)
