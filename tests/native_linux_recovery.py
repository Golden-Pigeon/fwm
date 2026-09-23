#!/usr/bin/env python3
"""Test native recovery against an explicitly selected disposable Linux SSH guest.

The SSH alias must resolve directly to loopback, authenticate as an unprivileged
user, and set FWM_REMOTE_STATE_DIR=/tmp/fwm-remote-state. Python runs only on the
client: guest inspection uses its shell and procfs; forwarding is tested with
OpenSSH's -W channel. Two isolated fwm managers exercise recovery and ownership.
"""
import argparse
import asyncio
import contextlib
import json
from pathlib import Path
import secrets
import shlex
import socket
import tempfile
import time
import uuid

from linux_recovery_smoke import FaultProxy, eventually, process


def json_records(text):
    """Accept both newline-delimited and adjacent JSON lease objects."""
    decoder = json.JSONDecoder()
    records = []
    while text := text.lstrip():
        record, end = decoder.raw_decode(text)
        records.append(record)
        text = text[end:]
    return records


def session_pid(record):
    return int(record.get("session_pid") or record["session"]["pid"])


def helper_pid(record):
    return int(record.get("helper_pid") or record["helper"]["pid"])


def session_uid(record):
    return int(record["session_uid"] if "session_uid" in record else record["session"]["uid"])


def listeners(text):
    result = set()
    for line in text.splitlines():
        fields = line.split()
        if len(fields) > 9 and fields[0].endswith(":") and fields[3] == "0A":
            result.add(int(fields[1].split(":")[1], 16))
    return result


async def run(args):
    ssh_base = ["ssh", "-F", str(args.ssh_config), "-o", "BatchMode=yes"]
    resolved = (await process("ssh", "-G", "-F", args.ssh_config, args.alias))[1]
    fields = {}
    for line in resolved.splitlines():
        key, _, value = line.partition(" ")
        fields.setdefault(key, value)
    assert fields["hostname"] in ("127.0.0.1", "localhost", "::1"), "only disposable loopback guests are allowed"
    assert fields.get("proxycommand", "none") == "none" and fields.get("proxyjump", "none") == "none", "fixture must connect directly to loopback"
    assert fields.get("controlmaster", "false") in ("false", "no"), "fixture must not share an existing SSH master"

    async def remote(command):
        return (await process(*ssh_base, args.alias, command, timeout=15))[1]

    fact_lines = (await remote("id -u\nuname -s\nuname -r\ncat /run/sshd.pid\nprintf '%s\\n' \"$FWM_REMOTE_STATE_DIR\"\ncommand -v python3 || true")).splitlines()
    assert len(fact_lines) >= 5, fact_lines
    facts = dict(uid=int(fact_lines[0]), system=fact_lines[1], kernel=fact_lines[2],
                 master=int(fact_lines[3]), registry=fact_lines[4],
                 guest_python=fact_lines[5:] or None)
    assert facts["system"] == "Linux" and facts["uid"] != 0, facts
    assert facts["registry"] == "/tmp/fwm-remote-state", facts
    print("Linux native fixture:", json.dumps(facts), flush=True)

    occupied = listeners(await remote("cat /proc/net/tcp"))
    remote_ports = []
    while len(remote_ports) != 3:
        candidate = 20000 + secrets.randbelow(30000)
        if candidate not in occupied and candidate not in remote_ports:
            remote_ports.append(candidate)
    remote_port, sentinel_port, manual_port = remote_ports
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
    with tempfile.TemporaryDirectory(prefix="fwm-native-linux-recovery-") as directory:
        root = Path(directory)
        known = root / "known_hosts"
        isolated_ssh_config = root / "ssh_config"
        isolated_ssh_config.write_text("Host *\n    GlobalKnownHostsFile none\n    IdentityAgent none\n")
        manual = None
        trusted_path = Path(shlex.split(fields["userknownhostsfile"])[0]).expanduser()
        trusted = trusted_path.read_text().splitlines()
        key_lines = [line for line in trusted if line and not line.startswith("#")]
        assert key_lines and all(not line.startswith("@") for line in key_lines), "fixture needs plain trusted host keys"
        known.write_text("\n".join(f"[127.0.0.1]:{proxy_port} " + line.split(" ", 1)[1]
                                   for line in key_lines) + "\n")
        identity = Path(fields["identityfile"]).expanduser()

        async def cli(*arguments, other=False, check=True):
            return await process(args.binary, "--config-dir", root / ("sentinel" if other else "manager"),
                                 *arguments, check=check)

        async def status(other=False):
            return json.loads((await cli("status", "--json", other=other))[1])["forwards"]

        async def state(name, other=False):
            return next(row for row in await status(other) if row["name"] == name)

        async def lease(port=remote_port):
            rows = json_records(await remote("cat /tmp/fwm-remote-state/leases/*/*.json 2>/dev/null || true"))
            return next((row for row in rows if row["listen_port"] == port and row["phase"] == "confirmed"), None)

        async def probe(pid, port=remote_port):
            # The process may have exited, so absence is expected, not an SSH failure.
            text = await remote(f"cat /proc/{int(pid)}/stat 2>/dev/null || true\nprintf '\\n'\ncat /proc/net/tcp")
            process_lines = [line for line in text.splitlines() if line.startswith(f"{int(pid)} (")]
            alive = bool(process_lines and process_lines[0].rsplit(") ", 1)[1].split()[0] not in ("Z", "X"))
            return dict(alive=alive, occupied=port in listeners(text))

        async def legacy_record(record):
            # Recreate the old Python lease shape using this live test
            # connection's exact process and socket identities, with no Python
            # executed on the guest and no fabricated ownership evidence.
            async def identity(pid):
                lines = (await remote(f"cat /proc/sys/kernel/random/boot_id\ncat /proc/{pid}/stat\ncat /proc/{pid}/status")).splitlines()
                stat = lines[1]
                close = stat.rindex(")")
                values = stat[close + 2:].split()
                uid = int(next(line for line in lines[2:] if line.startswith("Uid:")).split()[1])
                return dict(pid=pid, ppid=int(values[1]), birth=lines[0] + ":" + values[19],
                            uid=uid, name=stat[stat.index("(") + 1:close], state=values[0])

            session, helper = await identity(session_pid(record)), await identity(helper_pid(record))
            assert session["uid"] == helper["uid"] == facts["uid"]
            registered_birth = record.get("session_birth") or record["session"]["birth"]
            assert session["birth"] == registered_birth, "test session changed before lease migration"
            sockets = {}
            for line in (await remote("cat /proc/net/tcp")).splitlines()[1:]:
                values = line.split()
                if len(values) < 10:
                    continue

                def endpoint(value):
                    host, port = value.split(":")
                    return [socket.inet_ntoa(bytes.fromhex(host)[::-1]), int(port, 16)]

                sockets[int(values[9])] = dict(local=endpoint(values[1]), remote=endpoint(values[2]),
                                               listening=values[3] == "0A", uid=int(values[7]), inode=values[9])
            session_socket = sockets[int(record["session_proof"]["inode"])]
            listener_socket = sockets[int(record["listener_proof"]["inode"])]
            assert session_socket["uid"] == record["session_proof"]["uid"]
            assert listener_socket["uid"] == record["listener_proof"]["uid"]
            assert not session_socket["listening"] and listener_socket["listening"]
            assert listener_socket["local"] == [record["listen_host"], record["listen_port"]]
            nested = {key: record[key] for key in ("protocol", "phase", "owner_id", "rule_id", "generation",
                                                   "session_id", "listen_host", "listen_port", "registration",
                                                   "listener_absent_at_claim")}
            nested.update(session=session, helper=helper,
                          transport=[*session_socket["remote"], *session_socket["local"]],
                          session_proof=dict(source=record["session_proof"]["source"], socket=session_socket),
                          listener_proof=dict(source=record["listener_proof"]["source"], sockets=[listener_socket]))
            owner, rule = str(uuid.UUID(record["owner_id"])), str(uuid.UUID(record["rule_id"]))
            path = f"/tmp/fwm-remote-state/leases/{owner}/{rule}.json"
            temporary = path + ".native-e2e-migration"
            command = f"set -e\numask 077\ncat > {shlex.quote(temporary)}\nchmod 600 {shlex.quote(temporary)}\nmv -f {shlex.quote(temporary)} {shlex.quote(path)}"
            child = await asyncio.create_subprocess_exec(*ssh_base, args.alias, command,
                                                       stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                                                       stderr=asyncio.subprocess.PIPE)
            try:
                stdout, stderr = await asyncio.wait_for(child.communicate((json.dumps(nested) + "\n").encode()), 15)
                assert child.returncode == 0, (stdout, stderr)
            finally:
                if child.returncode is None:
                    child.kill()
                    await child.communicate()
            assert await lease() == nested
            return nested

        async def echo_remote(port):
            payload = b"native-linux-recovery-echo\n"
            child = await asyncio.create_subprocess_exec(*ssh_base, "-W", f"127.0.0.1:{port}", args.alias,
                                                       stdin=asyncio.subprocess.PIPE,
                                                       stdout=asyncio.subprocess.PIPE,
                                                       stderr=asyncio.subprocess.PIPE)
            try:
                stdout, stderr = await asyncio.wait_for(child.communicate(payload), 10)
                assert child.returncode == 0 and stdout == payload, (port, child.returncode, stdout, stderr)
            finally:
                if child.returncode is None:
                    child.kill()
                    await child.communicate()
            return True

        async def sentinel_intact(original):
            current = await lease(sentinel_port)
            assert current and current["session_id"] == original["session_id"], (original, current)
            assert (await probe(session_pid(original), sentinel_port))["alive"]
            assert (await probe(facts["master"], 22))["alive"]
            assert (await state("sentinel", other=True))["state"] == "established"
            assert await echo_remote(sentinel_port)

        async def replacement(previous):
            current, current_state = await lease(), await state("remote")
            if current_state["state"] == "needs_attention":
                raise RuntimeError(json.dumps(current_state))
            return current if (current and current["generation"] > previous["generation"]
                               and session_pid(current) != session_pid(previous)
                               and current_state["state"] == "established") else None

        try:
            await cli("server", "add", "linux", "--host", "127.0.0.1", "--port", str(proxy_port),
                      "--user", fields["user"], "--identity", identity, "--known-hosts", known,
                      "--ssh-config", isolated_ssh_config)
            await cli("add", "remote", "--server", "linux", "--remote", "--src", str(remote_port),
                      "--tgt", str(target_port), "--wait", "--timeout", "20s")
            old = await lease()
            assert old and session_uid(old) == facts["uid"], old
            assert (await state("remote"))["state"] == "established"
            assert len(proxy.connections) == 1, proxy.connections
            assert await echo_remote(remote_port)
            print("PASS native remote established and echo:", json.dumps(dict(pid=session_pid(old), generation=old["generation"], port=remote_port,
                                                                            transport_uid=old.get("session_proof", {}).get("uid"))), flush=True)

            await cli("server", "add", "linux", "--host", fields["hostname"], "--port", fields["port"],
                      "--user", fields["user"], "--identity", identity, "--known-hosts", trusted_path,
                      "--ssh-config", isolated_ssh_config, other=True)
            await cli("add", "sentinel", "--server", "linux", "--remote", "--src", str(sentinel_port),
                      "--tgt", str(target_port), "--wait", "--timeout", "20s", other=True)
            sentinel = await lease(sentinel_port)
            assert sentinel and sentinel["owner_id"] != old["owner_id"], sentinel
            await sentinel_intact(sentinel)

            # Manager B must not reclaim A's listener even while B has its own
            # established rule on the same server and under the same Unix uid.
            await cli("add", "foreign", "--server", "linux", "--remote", "--src", str(remote_port),
                      "--tgt", str(target_port), "--wait", "--timeout", "8s", other=True, check=False)
            foreign = await state("foreign", other=True)
            assert foreign["state"] != "established" and "unmanaged_conflict" in foreign["last_error"], foreign
            assert (await probe(session_pid(old)))["alive"]
            assert (await lease())["session_id"] == old["session_id"]
            assert await echo_remote(remote_port)
            await sentinel_intact(sentinel)
            await cli("down", "foreign", other=True)
            print("PASS other manager cannot reclaim the active owner; independent sentinel intact", flush=True)

            # A manually started SSH tunnel has no fwm lease and must remain
            # untouched, including when the same login user requests its port.
            manual = await asyncio.create_subprocess_exec(
                *ssh_base, "-o", "ExitOnForwardFailure=yes", "-N", "-R",
                f"127.0.0.1:{manual_port}:127.0.0.1:{target_port}", args.alias,
                stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE)

            async def manual_ready():
                assert manual.returncode is None, "fixture manual SSH forward exited"
                return (await probe(facts["master"], manual_port))["occupied"]

            await eventually(manual_ready, 15)
            assert await echo_remote(manual_port)
            await cli("add", "manual-conflict", "--server", "linux", "--remote", "--src", str(manual_port),
                      "--tgt", str(target_port), "--wait", "--timeout", "8s", other=True, check=False)
            conflict = await state("manual-conflict", other=True)
            assert conflict["state"] != "established" and "unmanaged_conflict" in conflict["last_error"], conflict
            assert manual.returncode is None and await echo_remote(manual_port)
            assert await lease(manual_port) is None
            assert await echo_remote(remote_port)
            await sentinel_intact(sentinel)
            await cli("down", "manual-conflict", other=True)
            print("PASS unmanaged manual SSH forward survives conflicting fwm claim and still echoes", flush=True)

            old = await legacy_record(old)
            assert "session_pid" not in old and isinstance(old["session_proof"]["socket"]["inode"], str)
            print("PASS same live lease converted to Python nested schema with actual process/socket identities", flush=True)

            active = proxy.connections[0]
            proxy.gate.clear()
            active["blocked"] = True
            started = time.monotonic()

            async def lost():
                return active["orphaned"] and (await state("remote"))["state"] != "established"

            await eventually(lost, 45)
            stale = await probe(session_pid(old))
            assert stale == dict(alive=True, occupied=True), stale
            assert not active["closed"], "fault proxy must retain the old upstream SSH socket"
            await sentinel_intact(sentinel)
            print("PASS blackhole leaves old server session alive and port occupied:", json.dumps(stale), flush=True)
            proxy.gate.set()
            new = await eventually(lambda: replacement(old), 40)
            assert not (await probe(session_pid(old)))["alive"]
            assert await echo_remote(remote_port)
            assert manual.returncode is None and await echo_remote(manual_port)
            await sentinel_intact(sentinel)
            print("PASS nested Python lease actively reclaimed/rebound; echo and master/sentinel intact:",
                  json.dumps(dict(old_pid=session_pid(old), new_pid=session_pid(new),
                                  old_generation=old["generation"], new_generation=new["generation"],
                                  seconds=round(time.monotonic() - started, 2))), flush=True)

            await remote(f"kill -TERM {helper_pid(new)}")
            renewed = await eventually(lambda: replacement(new), 25)
            assert not (await probe(helper_pid(new)))["alive"]
            assert not (await probe(session_pid(new)))["alive"]
            assert await echo_remote(remote_port)
            await sentinel_intact(sentinel)
            print("PASS killed helper triggers automatic fresh session and echo:",
                  json.dumps(dict(old_generation=new["generation"], new_generation=renewed["generation"])), flush=True)

            await cli("down", "remote")

            async def released():
                return await lease() is None and not (await probe(session_pid(renewed)))["occupied"]

            await eventually(released)
            await sentinel_intact(sentinel)
            await cli("up", "remote", "--wait", "--timeout", "20s")
            final = await lease()
            assert final and final["generation"] > renewed["generation"], final
            assert await echo_remote(remote_port)
            await sentinel_intact(sentinel)
            await cli("down", "remote")
            await eventually(released)
            print("PASS down releases lease/listener; up creates a fresh verified session", flush=True)
        except Exception:
            for other in (False, True):
                with contextlib.suppress(Exception):
                    print(f"FAIL manager other={other} status:", (await cli("status", "--json", other=other))[1], flush=True)
                    print(f"FAIL manager other={other} logs:", (await cli("logs", "--json", other=other))[1], flush=True)
            raise
        finally:
            proxy.gate.set()
            for other in (False, True):
                with contextlib.suppress(Exception):
                    await cli("daemon", "stop", other=other, check=False)
            if manual is not None and manual.returncode is None:
                manual.terminate()
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(manual.communicate(), 5)
                if manual.returncode is None:
                    manual.kill()
                    await manual.communicate()
            await proxy.close()
            service.close()
            target.close()
            await service.wait_closed()
            await target.wait_closed()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ssh-config", required=True, type=Path)
    parser.add_argument("--alias", required=True)
    parser.add_argument("--binary", default="target/release/fwm", type=Path)
    options = parser.parse_args()
    options.binary = options.binary.resolve()
    options.ssh_config = options.ssh_config.resolve()
    asyncio.run(run(options))
