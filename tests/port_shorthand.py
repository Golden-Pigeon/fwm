"""Port syntax checks exercised against the real SSH fixture in smoke.py."""
import socket
import random


def free_block(count=4):
    """Find consecutive free loopback ports without assuming the ephemeral range."""
    for _ in range(100):
        sockets = []
        try:
            first = socket.socket()
            sockets.append(first)
            first.bind(("127.0.0.1", random.randint(12000, 28000)))
            start = first.getsockname()[1]
            if start + count - 1 > 65535:
                continue
            for port in range(start + 1, start + count):
                sock = socket.socket()
                sockets.append(sock)
                sock.bind(("127.0.0.1", port))
            return start
        except OSError:
            continue
        finally:
            for sock in sockets:
                sock.close()
    raise AssertionError("could not reserve a consecutive test port range")


async def check_port_shorthand(cli, rpc, echo_test, echo_port):
    # With local and remote on the same test machine, same-port forwarding
    # would loop back to its own listener. Validate those mappings disabled,
    # and exercise real L/R traffic with multiple source ports to one target.
    async def config():
        reply = await rpc("get_config")
        assert reply["ok"], reply
        return reply["data"]

    start = free_block()
    before = await config()
    await cli("add", "same", "--server", "test", "--local", "--port",
              f"{start}-{start+1},{start+3},{start}", "--disabled")
    saved = await config()
    expected_ports = [start, start + 1, start + 3]
    assert saved["revision"] == before["revision"] + 1
    rules = [f for f in saved["forwards"] if f["name"].startswith("same-")]
    assert sorted(f["name"] for f in rules) == [f"same-{p}" for p in expected_ports]
    for rule in rules:
        port = int(rule["name"].rsplit("-", 1)[1])
        assert rule["listen"] == f"127.0.0.1:{port}"
        assert rule["target"] == f"localhost:{port}"
        assert rule["desired_state"] == "stopped"

    # A clash in one generated name must leave no new rule behind.
    before = await config()
    code, _, _ = await cli("add", "same", "--server", "test", "--remote", "--port",
                           f"{start+1}-{start+2}", "--disabled", success=False)
    assert code != 0
    assert await config() == before
    for port in expected_ports:
        await cli("remove", f"same-{port}")

    for direction in ["local", "remote"]:
        start = free_block()
        source_ports = [start, start + 1, start + 3]
        before = await config()
        await cli("add", f"multi-{direction}", "--server", "test", f"--{direction}",
                  "--src", f"{start}-{start+1},{start+3}", "--tgt", str(echo_port),
                  "--wait", "--timeout", "15s")
        saved = await config()
        assert saved["revision"] == before["revision"] + 1
        for port in source_ports:
            await echo_test(port, f"{direction}:{port}".encode())
        for port in source_ports:
            await cli("remove", f"multi-{direction}-{port}")
    print("PASS port lists/ranges, deduplication, atomic creation and L/R many-to-one traffic", flush=True)
