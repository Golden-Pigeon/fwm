"""Exercise actual trust prompts against the smoke suite's temporary sshd.

Every case uses an isolated manager, SSH config and known_hosts. PTYs are Unix
only. Blocking PTY reads run on a worker so the smoke SSH proxy keeps pumping.
"""
import asyncio
import contextlib
import json
import os
import pty
import select
import subprocess
import time


def run_prompt(arguments, answer):
    master, slave = pty.openpty()
    process = None
    try:
        process = subprocess.Popen(arguments, stdin=slave, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, close_fds=True)
        os.close(slave)
        slave = None
        buffers = {process.stdout.fileno(): bytearray(), process.stderr.fileno(): bytearray()}
        streams = set(buffers)
        answered = False
        deadline = time.monotonic() + 20
        while streams:
            assert time.monotonic() < deadline, ("trust prompt timed out", buffers)
            readable, _, _ = select.select(list(streams), [], [], 0.1)
            for descriptor in readable:
                data = os.read(descriptor, 65536)
                if not data:
                    streams.remove(descriptor)
                    continue
                buffers[descriptor].extend(data)
            prompt = bytes(buffers[process.stderr.fileno()])
            if not answered and b"type 'yes' to trust it:" in prompt:
                os.write(master, answer.encode() + b"\n")
                answered = True
        code = process.wait(timeout=3)
        assert answered, ("interactive prompt missing", buffers)
        return code, bytes(buffers[process.stdout.fileno()]).decode(), bytes(buffers[process.stderr.fileno()]).decode()
    finally:
        if process is not None:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=3)
            process.stdout.close()
            process.stderr.close()
        if slave is not None:
            os.close(slave)
        os.close(master)


async def check_interactive_trust(binary, root, ssh_port, ssh_user, key, fingerprint):
    config_dir = root / "interactive-trust-manager"
    ssh_config = root / "interactive-trust-ssh-config"
    known_hosts = root / "interactive-trust-known-hosts"
    ssh_config.write_text(f"""Host prompt-alias
 HostName 127.0.0.1
 Port {ssh_port}
 User {ssh_user}
 IdentityFile {key}
 IdentitiesOnly yes
 IdentityAgent none
 UserKnownHostsFile {known_hosts}
 GlobalKnownHostsFile none
""")
    base = [str(binary), "--config-dir", str(config_dir)]
    trust = ["server", "trust", "prompt-alias", "--ssh-config", str(ssh_config)]

    async def invoke(*args):
        process = await asyncio.create_subprocess_exec(
            *base, *args, stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            stdout, stderr = await asyncio.wait_for(process.communicate(), 20)
            return process.returncode, stdout.decode(), stderr.decode()
        finally:
            if process.returncode is None:
                process.kill()
            await process.wait()

    try:
        code, out, error = await invoke(*trust)
        assert code != 0 and "--fingerprint" in error and fingerprint in out, (code, out, error)
        assert not known_hosts.exists()

        code, out, error = await invoke("--json", *trust)
        result = json.loads(error)
        assert code == 3 and not out and result["error"]["code"] == "trust_required", result
        assert result["data"]["fingerprint"] == fingerprint
        assert result["data"]["status"] == "unknown"
        assert not known_hosts.exists()

        code, out, error = await invoke("--json", *trust, "--fingerprint", "SHA256:incorrect")
        assert code != 0 and not out and "mismatch" in json.loads(error)["error"]["message"]
        assert not known_hosts.exists()

        for answer in ("no", "YES", ""):
            code, out, error = await asyncio.to_thread(run_prompt, base + trust, answer)
            assert code != 0 and not out and "was not trusted" in error, (answer, code, out, error)
            assert fingerprint in error and not known_hosts.exists()

        code, out, error = await asyncio.to_thread(run_prompt, base + trust, "yes")
        assert code == 0 and json.loads(out)["status"] == "trusted", (code, out, error)
        assert fingerprint in error
        first = known_hosts.read_bytes()
        assert len(first.splitlines()) == 1
        code, out, error = await invoke("--json", *trust, "--fingerprint", fingerprint)
        assert code == 0 and json.loads(out)["data"]["status"] == "trusted", (code, out, error)
        assert known_hosts.read_bytes() == first, "repeated trust duplicated known_hosts"
        code, out, error = await invoke("--json", "daemon", "status")
        assert code == 0 and not json.loads(out)["daemon_running"], (code, out, error)
        code, out, error = await invoke("--json", "server", "list")
        assert code == 0 and json.loads(out)["servers"] == [], (code, out, error)
        print("PASS interactive trust PTY yes/no, non-TTY/JSON refusal, fingerprint mismatch and idempotent trust", flush=True)
    finally:
        with contextlib.suppress(Exception):
            await invoke("daemon", "stop")
