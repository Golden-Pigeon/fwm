"""Exercise rule/group restarts on real SSH while sibling streams stay open."""
import asyncio
import json
from port_shorthand import free_block


async def check_live_controls(cli, rpc, echo_test, local_port, remote_port, target_port):
    local_reader, local_writer = await asyncio.open_connection("127.0.0.1", local_port)

    async def ping(reader, writer, message):
        writer.write(message)
        await writer.drain()
        assert await asyncio.wait_for(reader.readexactly(len(message)), 5) == message

    try:
        await ping(local_reader, local_writer, b"before-restarts")
        _, out, _ = await cli("--json", "retry", "local")
        result = json.loads(out)["data"]
        assert not result["affected"] and result["skipped"], result
        for name in ["remote", "socks"]:
            _, out, _ = await cli("--json", "restart", name, "--wait", "--timeout", "15s")
            result = json.loads(out)
            assert result["ok"] and result["data"]["ready"]
            await ping(local_reader, local_writer, f"after-{name}".encode())
        await echo_test(remote_port)

        start = free_block(2)
        await cli("add", "live-group", "--server", "test", "--local", "--src", f"{start}-{start+1}",
                  "--tgt", str(target_port), "--wait", "--timeout", "15s")
        before = (await rpc("get_config"))["data"]["revision"]
        await cli("restart", "--group", "live-group", "--wait", "--timeout", "15s")
        assert (await rpc("get_config"))["data"]["revision"] == before + 1
        await echo_test(start)
        await echo_test(start + 1)
        await ping(local_reader, local_writer, b"after-group")
        await cli("remove", "live-group")

        remote_reader, remote_writer = await asyncio.open_connection("127.0.0.1", remote_port)
        try:
            await ping(remote_reader, remote_writer, b"remote-sibling")
            await cli("restart", "local", "--wait", "--timeout", "15s")
            assert await asyncio.wait_for(local_reader.read(), 5) == b""
            await ping(remote_reader, remote_writer, b"local-was-restarted")
            await echo_test(local_port)
        finally:
            remote_writer.close()
        print("PASS rule/group restarts isolate sibling streams; retry reports skips; wait JSON is one final result", flush=True)
    finally:
        local_writer.close()
