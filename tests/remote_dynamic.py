"""Reverse SOCKS5 traffic and lifecycle checks against smoke.py's real sshd."""
import asyncio
import contextlib
import ipaddress
import os
import socket
import struct

from remote_recovery import record_for


async def socks_connect(proxy_port, host, target_port):
    """Return the SOCKS reply and stream, preserving domain names on the wire."""
    reader, writer = await asyncio.open_connection("127.0.0.1", proxy_port)
    try:
        writer.write(b"\x05\x01\x00")
        await writer.drain()
        assert await asyncio.wait_for(reader.readexactly(2), 5) == b"\x05\x00"
        try:
            address = ipaddress.ip_address(host)
        except ValueError:
            encoded = host.encode("ascii")
            destination = b"\x03" + bytes([len(encoded)]) + encoded
        else:
            destination = bytes([1 if address.version == 4 else 4]) + address.packed
        writer.write(b"\x05\x01\x00" + destination + struct.pack(">H", target_port))
        await writer.drain()
        head = await asyncio.wait_for(reader.readexactly(4), 5)
        assert head[0] == 5 and head[2] == 0, head
        if head[3] == 3:
            length = (await asyncio.wait_for(reader.readexactly(1), 5))[0]
        else:
            assert head[3] in {1, 4}, head
            length = 4 if head[3] == 1 else 16
        await asyncio.wait_for(reader.readexactly(length + 2), 5)
        return head[1], reader, writer
    except BaseException:
        writer.close()
        with contextlib.suppress(OSError):
            await writer.wait_closed()
        raise


async def socks_echo(proxy_port, target_port, host="localhost", payload=b"reverse-socks\x00\xff"):
    code, reader, writer = await socks_connect(proxy_port, host, target_port)
    try:
        assert code == 0, code
        writer.write(payload)
        await writer.drain()
        assert await asyncio.wait_for(reader.readexactly(len(payload)), 10) == payload
        # Exercise EOF in the remote-to-local direction while the opposite
        # half still carries data across the forwarded SSH channel.
        writer.write_eof()
        assert await asyncio.wait_for(reader.read(), 5) == b""
        return True
    finally:
        writer.close()
        with contextlib.suppress(OSError):
            await writer.wait_closed()


async def check_remote_dynamic(cli, rpc, status, eventually, remote_state,
                               proxy_port, target_port, sibling_port):
    await cli("add", "remote-socks", "--server", "test", "--remote-dynamic",
              f"127.0.0.1:{proxy_port}", "--wait", "--timeout", "15s")
    config = (await rpc("get_config"))["data"]
    rule = next(rule for rule in config["forwards"] if rule["name"] == "remote-socks")
    assert rule["kind"] == "remote_dynamic", rule
    assert rule["listen"] == f"127.0.0.1:{proxy_port}", rule
    assert "target" not in rule, rule
    lease = record_for(remote_state, proxy_port)
    assert lease, "reverse SOCKS listener must be owned by the remote sshd lease"

    await socks_echo(proxy_port, target_port)
    await socks_echo(proxy_port, target_port, host="127.0.0.1", payload=os.urandom(256 * 1024))

    # Close an actual listener before connecting: on macOS a socket left
    # bound without listen() drops SYN packets instead of refusing them.
    with socket.socket() as absent_target:
        absent_target.bind(("127.0.0.1", 0))
        absent_target.listen()
        absent_port = absent_target.getsockname()[1]
    code, reader, writer = await socks_connect(proxy_port, "127.0.0.1", absent_port)
    try:
        assert code == 5, f"expected SOCKS connection refused, got {code}"
        assert await asyncio.wait_for(reader.read(), 5) == b""
    finally:
        writer.close()
        with contextlib.suppress(OSError):
            await writer.wait_closed()
    assert (await status())["remote-socks"]["state"] == "established"
    await socks_echo(proxy_port, target_port)

    sibling_reader, sibling_writer = await asyncio.open_connection("127.0.0.1", sibling_port)

    async def sibling_ping():
        payload = b"reverse-socks-control-isolated"
        sibling_writer.write(payload)
        await sibling_writer.drain()
        assert await asyncio.wait_for(sibling_reader.readexactly(len(payload)), 5) == payload

    try:
        code, reader, writer = await socks_connect(proxy_port, "localhost", target_port)
        assert code == 0, code
        try:
            await cli("down", "remote-socks")
            assert await asyncio.wait_for(reader.read(), 5) == b""

            async def released():
                if record_for(remote_state, proxy_port) is not None:
                    return False
                try:
                    _, probe = await asyncio.open_connection("127.0.0.1", proxy_port)
                except ConnectionRefusedError:
                    # down requests asynchronous shutdown; the supervisor
                    # publishes Stopped after the lease and listener disappear.
                    return (await status())["remote-socks"]["state"] == "stopped"
                else:
                    probe.close()
                    await probe.wait_closed()
                    return False

            await eventually(released, description="reverse SOCKS stopped with remote listener and lease released")
            await sibling_ping()
        finally:
            writer.close()
            with contextlib.suppress(OSError):
                await writer.wait_closed()

        await cli("up", "remote-socks", "--wait", "--timeout", "15s")
        await socks_echo(proxy_port, target_port)
        before_restart = record_for(remote_state, proxy_port)[1]
        code, reader, writer = await socks_connect(proxy_port, "localhost", target_port)
        assert code == 0, code
        try:
            await cli("restart", "remote-socks", "--wait", "--timeout", "15s")
            assert await asyncio.wait_for(reader.read(), 5) == b""
        finally:
            writer.close()
            with contextlib.suppress(OSError):
                await writer.wait_closed()
        replacement = record_for(remote_state, proxy_port)
        assert replacement and replacement[1]["generation"] > before_restart["generation"]
        await socks_echo(proxy_port, target_port)
        await sibling_ping()
    finally:
        sibling_writer.close()
        with contextlib.suppress(OSError):
            await sibling_writer.wait_closed()
    print("PASS reverse SOCKS5 IPv4/domain, large half-close, refusal, stop/up and restart isolation", flush=True)
