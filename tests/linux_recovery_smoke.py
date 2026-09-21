#!/usr/bin/env python3
"""Exercise verified cleanup on a disposable Linux SSH guest exposed on loopback.

Requires an explicit isolated SSH config/alias. Refuses non-loopback SSH targets.
The fixture must set FWM_REMOTE_STATE_DIR=/tmp/fwm-remote-state for this test user.
"""
import argparse
import asyncio
import contextlib
import json
from pathlib import Path
import shlex
import socket
import tempfile
import time


async def process(*args, check=True, timeout=45):
    child = await asyncio.create_subprocess_exec(*map(str, args), stdout=asyncio.subprocess.PIPE,
                                                 stderr=asyncio.subprocess.PIPE)
    stdout, stderr = await asyncio.wait_for(child.communicate(), timeout)
    if check and child.returncode:
        raise AssertionError(f"{args}: {stdout.decode()} {stderr.decode()}")
    return child.returncode, stdout.decode(), stderr.decode()


async def eventually(check, timeout=35):
    deadline, last = time.monotonic() + timeout, None
    while time.monotonic() < deadline:
        try:
            value = await check()
            if value:
                return value
        except (OSError, AssertionError, ValueError) as error:
            last = error
        await asyncio.sleep(0.2)
    raise AssertionError(f"condition timed out: {last}")


class FaultProxy:
    def __init__(self, host, port):
        self.host, self.port = host, port
        self.gate = asyncio.Event()
        self.gate.set()
        self.connections = []
        self.tasks = set()

    async def handle(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        upstream = None
        try:
            await self.gate.wait()
            remote, upstream = await asyncio.open_connection(self.host, self.port)
            entry = {"blocked": False, "orphaned": False, "closed": False,
                     "peer": upstream.get_extra_info("sockname")[1]}
            self.connections.append(entry)

            async def pump(source, destination, client=False):
                while data := await source.read(65536):
                    if not entry["blocked"]:
                        destination.write(data)
                        await destination.drain()
                if client and entry["blocked"]:
                    entry["orphaned"] = True

            tasks = [asyncio.create_task(pump(reader, upstream, True)),
                     asyncio.create_task(pump(remote, writer))]
            try:
                await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                if entry["orphaned"]:
                    # Hold the upstream TCP open after fwm gives up. Only the
                    # server killing its old session can close this socket now.
                    await tasks[1]
            finally:
                for child in tasks:
                    child.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
                entry["closed"] = True
        except (OSError, asyncio.CancelledError):
            pass
        finally:
            writer.close()
            if upstream:
                upstream.close()
            self.tasks.discard(task)

    async def close(self):
        self.gate.set()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)


async def run(args):
    ssh = ["ssh", "-F", str(args.ssh_config), "-o", "BatchMode=yes", args.alias]
    resolved = (await process("ssh", "-G", "-F", args.ssh_config, args.alias))[1]
    fields = {}
    for line in resolved.splitlines():
        key, _, value = line.partition(" ")
        fields.setdefault(key, value)
    assert fields["hostname"] in ("127.0.0.1", "localhost", "::1"), "only disposable loopback guests are allowed"
    assert fields.get("proxycommand", "none") == "none" and fields.get("proxyjump", "none") == "none", "fixture must connect directly to loopback"

    async def remote(code):
        command = "python3 -c " + shlex.quote(code)
        return json.loads((await process(*ssh, command))[1])

    facts = await remote("import os,platform,json; from pathlib import Path; "
                         "print(json.dumps(dict(uid=os.geteuid(),system=platform.system(),"
                         "kernel=platform.release(),master=int(Path('/run/sshd.pid').read_text()),"
                         "registry=os.environ.get('FWM_REMOTE_STATE_DIR'))))")
    assert facts["system"] == "Linux" and facts["uid"] != 0, facts
    assert facts["registry"] == "/tmp/fwm-remote-state", facts
    print("Linux fixture:", json.dumps(facts), flush=True)
    listener_code = "import socketserver; s=socketserver.ThreadingTCPServer(('127.0.0.1',0),type('H',(socketserver.BaseRequestHandler,),{'handle':lambda self:self.request.sendall(self.request.recv(4096))})); print(s.server_address[1],flush=True); s.serve_forever()"
    unrelated = await remote("import subprocess,sys,json; p=subprocess.Popen([sys.executable,'-u','-c',"
                             + repr(listener_code) + "],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,"
                             "stderr=subprocess.DEVNULL,start_new_session=True); "
                             "print(json.dumps(dict(pid=p.pid,port=int(p.stdout.readline()))))")
    remote_port = await remote("import socket,json; s=socket.socket(); s.bind(('127.0.0.1',0)); print(json.dumps(s.getsockname()[1]))")
    proxy = FaultProxy(fields["hostname"], int(fields["port"]))
    service = await asyncio.start_server(proxy.handle, "127.0.0.1", 0)
    proxy_port = service.sockets[0].getsockname()[1]

    async def echo(reader, writer):
        try:
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
    target = await asyncio.start_server(echo, "127.0.0.1", 0)
    target_port = target.sockets[0].getsockname()[1]
    with tempfile.TemporaryDirectory(prefix="fwm-linux-recovery-") as directory:
        root = Path(directory)
        known = root / "known_hosts"
        # Copy the fixture's already trusted key; do not create a probe SSH session.
        trusted = Path(shlex.split(fields["userknownhostsfile"])[0]).read_text().splitlines()
        known.write_text("\n".join(f"[127.0.0.1]:{proxy_port} " + line.split(" ", 1)[1]
                                   for line in trusted if line and not line.startswith("#")) + "\n")

        async def cli(*arguments, other=False, check=True):
            return await process(args.binary, "--config-dir", root / ("other" if other else "manager"),
                                 *arguments, check=check)

        async def status(other=False):
            return json.loads((await cli("status", "--json", other=other))[1])["forwards"]

        async def lease():
            return await remote("import json; from pathlib import Path; rows=[json.loads(p.read_text()) for p in Path('/tmp/fwm-remote-state/leases').glob('*/*.json')]; "
                                f"print(json.dumps(next((r for r in rows if r['listen_port']=={remote_port} and r['phase']=='confirmed'),None)))")

        async def probe(pid, port=remote_port):
            return await remote("import socket,json; from pathlib import Path; "
                                f"p=Path('/proc/{pid}/stat'); alive=p.exists() and p.read_text().split(') ')[1][0]!='Z'; "
                                "s=socket.socket(); occupied=False\ntry:\n "
                                f"s.bind(('127.0.0.1',{port}))\nexcept OSError:\n occupied=True\n"
                                "print(json.dumps(dict(alive=alive,occupied=occupied)))")

        async def echo_remote(port):
            return await remote("import socket,json; "
                                f"s=socket.create_connection(('127.0.0.1',{port}),timeout=5); "
                                "s.sendall(b'verified-linux-echo'); print(json.dumps(s.recv(100)==b'verified-linux-echo'))")

        try:
            await cli("server", "add", "linux", "--host", "127.0.0.1", "--port", str(proxy_port),
                      "--user", fields["user"], "--identity", fields["identityfile"], "--known-hosts", known)
            await cli("add", "remote", "--server", "linux", "--remote", "--src", str(remote_port),
                      "--tgt", str(target_port), "--wait", "--timeout", "20s")
            old = await lease()
            assert old and old["session"]["uid"] == facts["uid"], old
            assert len(proxy.connections) == 1, proxy.connections
            assert await echo_remote(remote_port)
            print("PASS verified remote established:", json.dumps({"pid": old["session"]["pid"], "generation": old["generation"], "port": remote_port}), flush=True)

            # A different local manager gets a different owner identity. It must
            # report conflict, never reuse or kill the first manager's listener.
            await cli("add", "foreign", "--server", args.alias, "--ssh-config", args.ssh_config,
                      "--remote", "--src", str(remote_port), "--tgt", str(target_port),
                      "--wait", "--timeout", "8s", other=True, check=False)
            foreign = await status(other=True)
            assert foreign[0]["state"] != "established", foreign
            assert "unmanaged_conflict" in foreign[0]["last_error"], foreign
            assert (await probe(old["session"]["pid"]))["alive"]
            assert (await lease())["generation"] == old["generation"]
            assert await echo_remote(remote_port)
            await cli("daemon", "stop", other=True)
            print("PASS another manager cannot reclaim or kill the active owner's session", flush=True)

            active = proxy.connections[0]
            proxy.gate.clear()
            active["blocked"] = True
            started = time.monotonic()

            async def lost():
                return active["orphaned"] and (await status())[0]["state"] != "established"
            await eventually(lost, 40)
            stale = await probe(old["session"]["pid"])
            assert stale == {"alive": True, "occupied": True}, stale
            assert not active["closed"], "fault proxy must retain the upstream SSH socket"
            assert await echo_remote(unrelated["port"])
            print("PASS confirmed stale session and occupied port after client timeout:", json.dumps(stale), flush=True)
            proxy.gate.set()

            async def replaced():
                current = await lease()
                states = await status()
                if states and states[0]["state"] == "needs_attention":
                    raise RuntimeError(json.dumps(states))
                return current if current and current["generation"] > old["generation"] and current["session"]["pid"] != old["session"]["pid"] and states[0]["state"] == "established" else None
            new = await eventually(replaced, 35)
            assert not (await probe(old["session"]["pid"]))["alive"]
            assert (await probe(facts["master"], 22))["alive"]
            assert (await probe(unrelated["pid"], unrelated["port"]))["alive"]
            assert await echo_remote(remote_port) and await echo_remote(unrelated["port"])
            evidence = dict(old_pid=old["session"]["pid"], new_pid=new["session"]["pid"],
                            old_generation=old["generation"], new_generation=new["generation"],
                            recovery_seconds=round(time.monotonic()-started, 2), uid=facts["uid"])
            print("PASS automatic active reclaim/rebind and real echo; master/unrelated service intact:", json.dumps(evidence), flush=True)
            await remote(f"import os,signal,json; os.kill({new['helper']['pid']},signal.SIGTERM); print(json.dumps(True))")

            async def helper_recovered():
                current = await lease()
                states = await status()
                return current if current and current["generation"] > new["generation"] and current["session"]["pid"] != new["session"]["pid"] and states[0]["state"] == "established" else None
            renewed = await eventually(helper_recovered, 20)
            assert not (await probe(new["helper"]["pid"]))["alive"]
            assert not (await probe(new["session"]["pid"]))["alive"]
            assert await echo_remote(remote_port) and await echo_remote(unrelated["port"])
            assert (await probe(facts["master"], 22))["alive"]
            print("PASS killed helper triggers automatic fresh SSH/lease and echo:", json.dumps(dict(old_generation=new["generation"], new_generation=renewed["generation"], old_pid=new["session"]["pid"], new_pid=renewed["session"]["pid"])), flush=True)
            new = renewed
            await cli("down", "remote")

            async def released():
                return await lease() is None and not (await probe(new["session"]["pid"]))["occupied"]
            await eventually(released)
            await cli("up", "remote", "--wait", "--timeout", "20s")
            assert await echo_remote(remote_port)
            final = await lease()
            assert final["generation"] > new["generation"]
            await cli("down", "remote")
            await eventually(released)
            print("PASS normal down removes lease/listener; up establishes a fresh verified session", flush=True)
        except Exception:
            with contextlib.suppress(Exception):
                print("FAIL status:", (await cli("status", "--json"))[1], flush=True)
                print("FAIL logs:", (await cli("logs", "--json"))[1], flush=True)
            raise
        finally:
            proxy.gate.set()
            for other in (False, True):
                with contextlib.suppress(Exception):
                    await cli("daemon", "stop", other=other, check=False)
            await proxy.close()
            service.close()
            target.close()
            with contextlib.suppress(Exception):
                await remote(f"import os,signal,json; os.kill({unrelated['pid']},signal.SIGTERM); print(json.dumps(True))")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ssh-config", required=True, type=Path)
    parser.add_argument("--alias", required=True)
    parser.add_argument("--binary", default="target/debug/fwm", type=Path)
    options = parser.parse_args()
    options.binary = options.binary.resolve()
    options.ssh_config = options.ssh_config.resolve()
    asyncio.run(run(options))
