"""Private socket fixture: a live nonresponsive peer without a visible lock."""
import asyncio
import json
from pathlib import Path
import struct
import tempfile

ROOT = Path(__file__).resolve().parents[3]


async def main():
    with tempfile.TemporaryDirectory(prefix="fwm-postfix-ipc-") as temporary:
        config = Path(temporary)
        state = config / "state"
        state.mkdir(mode=0o700)
        requests = []
        complete = asyncio.Event()
        writers = []

        async def peer(reader, writer):
            writers.append(writer)
            try:
                size = struct.unpack(">I", await reader.readexactly(4))[0]
                requests.append(json.loads(await reader.readexactly(size)))
                await complete.wait()
            except (asyncio.IncompleteReadError, ConnectionError):
                pass
            finally:
                writer.close()

        server = await asyncio.start_unix_server(peer, path=str(state / "daemon.sock"))
        results = []
        try:
            for command in ["status", "stop", "status"]:
                process = await asyncio.create_subprocess_exec(
                    str(ROOT / "target/release/fwm"), "--config-dir", str(config), "--json", "daemon", command,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
                out, err = await asyncio.wait_for(process.communicate(), 30)
                results.append({"command": command, "exit": process.returncode,
                                "out": out.decode().strip(), "error": err.decode().strip()})
            assert json.loads(results[0]["out"])["daemon_state"] == "unresponsive"
            assert results[1]["exit"] == 0 and "Daemon stopped" in results[1]["out"]
            assert json.loads(results[2]["out"])["daemon_state"] == "unresponsive"
            print(json.dumps({"results": results, "requests": requests}, indent=2))
        finally:
            complete.set()
            server.close()
            await server.wait_closed()
            for writer in writers:
                writer.close()


asyncio.run(main())
