#!/usr/bin/env python3
"""Real OpenSSH integration smoke test (Unix; no third-party Python packages).

Run after cargo build: python3 tests/smoke.py [target/debug/fwm]
Uses only temporary keys/configs and loopback listeners. Requires sshd and
ssh-keygen. No existing SSH configuration or system service is changed.
"""
import asyncio
import contextlib
import getpass
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid
import random
from port_shorthand import check_port_shorthand
from intuitive_add import check_direct_alias
from remote_recovery import check_remote_recovery
from ux_live import check_live_controls
from trust_recovery import check_trust_recovery
from trust_interactive import check_interactive_trust
from ssh_refresh import check_ssh_refresh


def free_port():
    for _ in range(100):
        port = random.randint(12000, 28000)
        with socket.socket() as sock:
            try:
                sock.bind(("127.0.0.1", port))
                return port
            except OSError:
                pass
    raise AssertionError("could not reserve a test port")


async def eventually(check, timeout=25, description="condition"):
    deadline = time.monotonic() + timeout
    error = None
    while time.monotonic() < deadline:
        try:
            result = await check()
            if result:
                return result
        except (OSError, AssertionError, asyncio.IncompleteReadError) as exc:
            error = exc
        await asyncio.sleep(0.15)
    raise AssertionError(f"timed out waiting for {description}: {error}")


async def run(binary):
    sshd = shutil.which("sshd") or "/usr/sbin/sshd"
    keygen = shutil.which("ssh-keygen")
    assert Path(sshd).exists() and keygen, "sshd and ssh-keygen are required"
    with tempfile.TemporaryDirectory(prefix="fwm-smoke-") as directory:
        root = Path(directory)
        # A test-owned binary makes daemon restart tests independent of another
        # build replacing target/debug/fwm during parallel development.
        executable = root / "fwm-test-bin"
        shutil.copy2(binary, executable)
        binary = executable
        ssh_user = os.environ.get("FWM_SSH_TEST_USER", getpass.getuser())
        if ssh_user != getpass.getuser():
            # In Linux CI sshd runs as root and authenticates the unlocked
            # runner account. That account must traverse to its public key.
            root.chmod(0o711)
        remote_state = root / "remote-state"
        remote_state.mkdir(mode=0o700)
        if ssh_user != getpass.getuser():
            import pwd
            account = pwd.getpwnam(ssh_user)
            os.chown(remote_state, account.pw_uid, account.pw_gid)
        config_dir = root / "manager"
        client_config = root / "direct-host-ssh-config"
        client_config.write_text("Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n")
        logs = root / "logs"
        logs.mkdir()
        server_port, proxy_port, echo_port, local_port, remote_port, socks_port = [free_port() for _ in range(6)]
        key, host_key = root / "identity", root / "host_key"
        for path in [key, host_key]:
            subprocess.run([keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(path)], check=True)
        authorized = root / "authorized_keys"
        authorized.write_text(key.with_suffix(".pub").read_text())
        server_config = root / "sshd_config"
        server_config.write_text(f"""Port {server_port}
ListenAddress 127.0.0.1
HostKey {host_key}
PidFile {root / 'sshd.pid'}
AuthorizedKeysFile {authorized}
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin yes
AllowUsers {ssh_user}
AllowTcpForwarding yes
GatewayPorts clientspecified
ClientAliveInterval 0
SetEnv FWM_REMOTE_STATE_DIR={root / 'remote-state'}
LogLevel VERBOSE
""")
        # Trust tests intentionally make many pre-authentication inspections
        # from one loopback address. Newer sshd versions penalize that pattern;
        # disable it only for this disposable server. Older versions have no
        # such option, so probe support before adding it to the fixture config.
        penalty_option = subprocess.run(
            [sshd, "-t", "-f", str(server_config), "-o", "PerSourcePenalties=no"],
            capture_output=True,
        )
        if penalty_option.returncode == 0:
            with server_config.open("a") as file:
                file.write("PerSourcePenalties no\n")
        server_log = (logs / "sshd.log").open("w")
        server = subprocess.Popen([sshd, "-D", "-e", "-f", str(server_config)], stdout=server_log, stderr=server_log)
        blocked = False
        held_peers = set()
        proxy_tasks = set()
        allow_new_connections = asyncio.Event()
        allow_new_connections.set()

        async def echo(reader, writer):
            try:
                while data := await reader.read(65536):
                    writer.write(data)
                    await writer.drain()
            except (OSError, asyncio.CancelledError):
                pass
            finally:
                writer.close()

        async def proxy(reader, writer):
            task = asyncio.current_task()
            proxy_tasks.add(task)
            upstream = None
            try:
                remote_reader, upstream = await asyncio.open_connection("127.0.0.1", server_port)
                peer_port = upstream.get_extra_info("sockname")[1]
                await allow_new_connections.wait()
                orphaned = False

                async def pump(source, destination, from_client=False):
                    nonlocal orphaned
                    while data := await source.read(65536):
                        if not blocked and peer_port not in held_peers and not orphaned:
                            destination.write(data)
                            await destination.drain()
                    if from_client and peer_port in held_peers:
                        orphaned = True

                tasks = [asyncio.create_task(pump(reader, upstream, True)), asyncio.create_task(pump(remote_reader, writer))]
                try:
                    await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                    if orphaned:
                        # Deliberately keep the server-side TCP connection open:
                        # normal proxy EOF cleanup would hide stale sshd listeners.
                        await tasks[1]
                finally:
                    for task in tasks:
                        task.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
            except (OSError, asyncio.CancelledError):
                pass
            finally:
                writer.close()
                if upstream:
                    upstream.close()
                    with contextlib.suppress(OSError):
                        await upstream.wait_closed()
                proxy_tasks.discard(task)

        echo_server = await asyncio.start_server(echo, "127.0.0.1", echo_port)
        proxy_server = await asyncio.start_server(proxy, "127.0.0.1", proxy_port)

        async def cli(*args, success=True):
            process = await asyncio.create_subprocess_exec(str(binary), "--config-dir", str(config_dir), *args,
                                                          stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, stderr = await asyncio.wait_for(process.communicate(), 45)
            if success and process.returncode != 0:
                raise AssertionError(f"CLI {args}: {stderr.decode()} {stdout.decode()}")
            return process.returncode, stdout.decode(), stderr.decode()

        async def rpc(method, params=None, revision=None):
            reader, writer = await asyncio.open_unix_connection(config_dir / "state" / "daemon.sock")
            command = {"method": method}
            if params is not None:
                command["params"] = params
            request = {"api_version": 1, "request_id": str(uuid.uuid4()), "expected_revision": revision, "command": command}
            data = json.dumps(request).encode()
            writer.write(struct.pack(">I", len(data)) + data)
            await writer.drain()
            size = struct.unpack(">I", await reader.readexactly(4))[0]
            result = json.loads(await reader.readexactly(size))
            writer.close()
            await writer.wait_closed()
            return result

        async def status():
            reply = await rpc("status")
            assert reply["ok"], reply
            return {f["name"]: f for f in reply["data"]["forwards"]}

        async def all_ready(names):
            states = await status()
            return all(name in states and states[name]["state"] == "established" for name in names)

        async def echo_test(port, payload=b"fwm-integration\x00\xff"):
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.write(payload)
            await writer.drain()
            assert await asyncio.wait_for(reader.readexactly(len(payload)), 5) == payload
            writer.close()
            await writer.wait_closed()
            return True

        try:
            async def server_ready():
                if server.poll() is not None:
                    raise RuntimeError((logs / "sshd.log").read_text())
                reader, writer = await asyncio.open_connection("127.0.0.1", server_port)
                banner = await reader.readline()
                writer.close()
                return banner.startswith(b"SSH-")

            await eventually(server_ready, description="temporary sshd")
            # Explicit expected fingerprint exercises the real trust workflow.
            fingerprint = subprocess.check_output([keygen, "-lf", str(host_key.with_suffix(".pub"))], text=True).split()[1]
            known_hosts = root / "known_hosts"
            await cli("server", "add", "test", "--host", "127.0.0.1", "--user", ssh_user, "--port", str(proxy_port), "--identity", str(key), "--known-hosts", str(known_hosts), "--ssh-config", str(client_config))
            await cli("server", "trust", "test", "--fingerprint", fingerprint)
            # Configuration and trust are now truly offline. The subsequent RPC
            # tests explicitly opt into starting this isolated test daemon.
            await cli("daemon", "start")
            await check_direct_alias(cli, rpc, echo_test, root, proxy_port, ssh_user, key, known_hosts, echo_port)
            await cli("add", "local", "--server", "test", "--local", f"{local_port}:127.0.0.1:{echo_port}", "--wait", "--timeout", "15s")
            await cli("add", "remote", "--server", "test", "--remote", f"{remote_port}:127.0.0.1:{echo_port}", "--wait", "--timeout", "15s")
            await cli("add", "socks", "--server", "test", "--dynamic", str(socks_port), "--wait", "--timeout", "15s")
            await echo_test(local_port)
            await echo_test(remote_port)
            # Larger than SSH packets/windows; propagate local write EOF while
            # continuing to receive all data on the opposite half of the stream.
            for port in [local_port, remote_port]:
                payload = os.urandom(256 * 1024)
                half_reader, half_writer = await asyncio.open_connection("127.0.0.1", port)
                half_writer.write(payload)
                await half_writer.drain()
                half_writer.write_eof()
                assert await asyncio.wait_for(half_reader.readexactly(len(payload)), 10) == payload
                assert await asyncio.wait_for(half_reader.read(), 5) == b""
                half_writer.close()
            reader, writer = await asyncio.open_connection("127.0.0.1", socks_port)
            writer.write(b"\x05\x01\x00")
            await writer.drain()
            assert await reader.readexactly(2) == b"\x05\x00"
            domain = b"localhost"
            writer.write(b"\x05\x01\x00\x03" + bytes([len(domain)]) + domain + struct.pack(">H", echo_port))
            await writer.drain()
            head = await reader.readexactly(4)
            assert head[1] == 0, head
            await reader.readexactly((16 if head[3] == 4 else 4) + 2)
            writer.write(b"socks-works")
            await writer.drain()
            assert await reader.readexactly(11) == b"socks-works"
            writer.close()
            print("PASS real OpenSSH: local / remote / SOCKS5 forwarding", flush=True)

            await check_live_controls(cli, rpc, echo_test, local_port, remote_port, echo_port)
            await check_trust_recovery(cli, rpc, eventually, root, proxy_port, ssh_user,
                                       key, fingerprint, echo_port, local_port, binary, server_port)
            await check_interactive_trust(binary, root, proxy_port, ssh_user, key, fingerprint)
            await check_ssh_refresh(cli, rpc, echo_test, root, proxy_port, ssh_user, key, known_hosts, echo_port, local_port)
            if os.environ.get("FWM_SMOKE_UX_ONLY") == "1":
                return

            await check_remote_recovery(cli, rpc, echo_test, status, eventually,
                                        root / "remote-state", held_peers, remote_port, local_port,
                                        server, config_dir, binary, server_port, proxy_port,
                                        ssh_user, key, known_hosts, echo_port, allow_new_connections)

            await check_port_shorthand(cli, rpc, echo_test, echo_port)

            stable_reader, stable_writer = await asyncio.open_connection("127.0.0.1", local_port)
            stable_writer.write(b"before")
            await stable_writer.drain()
            assert await stable_reader.readexactly(6) == b"before"
            await cli("edit", "local", "--rename", "renamed")
            extra_port = free_port()
            await cli("add", "extra", "--server", "test", "--remote", f"{extra_port}:127.0.0.1:{echo_port}", "--wait", "--timeout", "15s")
            await echo_test(extra_port)
            await cli("remove", "extra")
            await cli("add", "extra-reused", "--server", "test", "--remote", f"{extra_port}:127.0.0.1:{echo_port}", "--wait", "--timeout", "15s")
            await echo_test(extra_port)
            failed_port, absent_target = free_port(), free_port()
            await cli("add", "unavailable", "--server", "test", "--remote", f"{failed_port}:127.0.0.1:{absent_target}", "--wait", "--timeout", "15s")
            bad_reader, bad_writer = await asyncio.open_connection("127.0.0.1", failed_port)
            assert await asyncio.wait_for(bad_reader.read(), 5) == b""
            bad_writer.close()
            assert (await status())["renamed"]["state"] == "established"
            await cli("remove", "unavailable")
            stable_writer.write(b"after")
            await stable_writer.drain()
            assert await asyncio.wait_for(stable_reader.readexactly(5), 5) == b"after"
            stable_writer.close()
            print("PASS add/remove remote rule and rename preserve sibling TCP stream", flush=True)

            # Compare-and-swap prevents concurrent UI/CLI writes from overwriting.
            config = (await rpc("get_config"))["data"]
            revision = config["revision"]
            request = {"selection": {"by": "forward", "value": "socks"}, "state": "stopped"}
            first, second = await asyncio.gather(rpc("set_desired", request, revision), rpc("set_desired", request, revision))
            assert sorted([first["ok"], second["ok"]]) == [False, True]
            failed = first if not first["ok"] else second
            assert failed["error"]["code"] == "revision_conflict", failed
            await cli("up", "socks", "--wait", "--timeout", "15s")
            print("PASS concurrent configuration revisions", flush=True)

            # Simulates a blackhole without firewall privileges or closing sockets.
            blocked = True
            started = time.monotonic()

            async def detected():
                return (await status())["renamed"]["state"] in {"backoff", "starting"}

            await eventually(detected, timeout=35, description="SSH blackhole detection")
            detection_time = time.monotonic() - started
            await cli("remove", "remote")
            blocked = False
            await eventually(lambda: all_ready(["renamed", "socks", "extra-reused"]), timeout=45, description="automatic recovery")
            await echo_test(local_port)
            await echo_test(extra_port)
            assert "remote" not in await status()
            print(f"PASS blackhole recovery; detected after {detection_time:.1f}s; deleted rule stays deleted", flush=True)

            # Manual invalid edits must not overwrite durable applied state.
            applied = (config_dir / "config.toml").read_text()
            (config_dir / "config.toml").write_text("invalid [[toml")
            code, _, _ = await cli("config", "reload", success=False)
            assert code != 0
            await echo_test(local_port)
            (config_dir / "config.toml").write_text(applied)
            await cli("down", "renamed")
            await cli("daemon", "restart")
            await eventually(lambda: all_ready(["socks"]), description="restart restores running rules")
            assert (await status())["renamed"]["state"] == "stopped"
            await cli("up", "renamed", "--wait", "--timeout", "15s")
            await echo_test(local_port)
            print("PASS invalid-config rollback and persistent manual stop across daemon restart", flush=True)
            await cli("doctor", "--server", "test")
            print("PASS authentication doctor", flush=True)
        except BaseException:
            print("\nTemporary sshd log:\n" + (logs / "sshd.log").read_text()[-6000:], file=sys.stderr)
            manager_log = config_dir / "state" / "daemon.log"
            if manager_log.exists():
                print("\nDaemon log:\n" + manager_log.read_text()[-6000:], file=sys.stderr)
            events_log = config_dir / "state" / "events.jsonl"
            if events_log.exists():
                print("\nEvent log:\n" + events_log.read_text()[-6000:], file=sys.stderr)
            raise
        finally:
            with contextlib.suppress(Exception):
                await cli("daemon", "stop", success=False)
            proxy_server.close()
            echo_server.close()
            await proxy_server.wait_closed()
            await echo_server.wait_closed()
            # Closing a listening server does not close accepted connections.
            # In particular, fault-injected orphan proxies can keep an sshd
            # helper alive and writing leases while TemporaryDirectory cleans up.
            remaining = list(proxy_tasks)
            for task in remaining:
                task.cancel()
            await asyncio.gather(*remaining, return_exceptions=True)
            server.terminate()
            with contextlib.suppress(subprocess.TimeoutExpired):
                await asyncio.to_thread(server.wait, 5)
            if server.poll() is None:
                server.kill()
                server.wait()
            server_log.close()


if __name__ == "__main__":
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/fwm").resolve()
    asyncio.run(run(binary))
