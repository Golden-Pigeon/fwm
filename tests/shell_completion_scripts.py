#!/usr/bin/env python3
"""Exercise the completion scripts with real Bash/Zsh line editors (no daemon)."""

import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
PROMPT = b"FWM_COMPLETION_READY> "


class Shell:
    def __init__(self, kind, directory, autoload=False):
        self.kind = kind
        self.directory = directory
        self.log = directory / "invocations.jsonl"
        self.candidates = directory / "candidates.json"
        self.candidates.write_text("[]")
        bindir = directory / "bin"
        bindir.mkdir()
        self.binary = bindir / "fwm"
        self.binary.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "with open(os.environ['FWM_COMPLETION_LOG'], 'a') as f:\n"
            "    f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if len(sys.argv) > 1 and sys.argv[1] == '__complete':\n"
            "    for candidate in json.load(open(os.environ['FWM_COMPLETION_CANDIDATES'])):\n"
            "        print(candidate)\n"
        )
        self.binary.chmod(0o755)
        setup = directory / "setup"
        lines = [
            f"export PATH={shlex.quote(str(bindir))}:$PATH",
            f"export FWM_COMPLETION_LOG={shlex.quote(str(self.log))}",
            f"export FWM_COMPLETION_CANDIDATES={shlex.quote(str(self.candidates))}",
            "PS1='FWM_COMPLETION_READY> '",
        ]
        if kind == "bash":
            lines += [f"source {shlex.quote(str(ROOT / 'completions/fwm.bash'))}",
                      "bind 'set bell-style none'"]
        else:
            if autoload:
                lines += [f"fpath=({shlex.quote(str(ROOT / 'completions'))} $fpath)"]
            lines += ["autoload -Uz compinit; compinit -D", "unsetopt beep"]
            if not autoload:
                lines += [f"source {shlex.quote(str(ROOT / 'completions/_fwm'))}"]
        setup.write_text("\n".join(lines) + "\n")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(directory)
            os.environ["TERM"] = "dumb"
            shell = shutil.which(kind)
            args = [shell, "--noprofile", "--norc", "-i"] if kind == "bash" else [shell, "-dfi"]
            os.execv(shell, args)
        os.write(self.fd, f"source {shlex.quote(str(setup))}\n".encode())
        self.read_prompt()

    def read_prompt(self):
        output = b""
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            readable, _, _ = select.select([self.fd], [], [], max(0, deadline - time.monotonic()))
            if readable:
                output += os.read(self.fd, 65536)
                if re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", output).endswith(PROMPT):
                    return output
        raise AssertionError(f"{self.kind} did not return to prompt: {output!r}")

    def complete(self, line, candidates, cursor=None, after_tab=""):
        self.candidates.write_text(json.dumps(candidates))
        self.log.write_text("")
        keys = line.encode()
        if cursor is not None:
            keys += b"\x1b[D" * (len(line) - cursor)
        os.write(self.fd, keys + b"\t" + after_tab.encode() + b"\n")
        screen = self.read_prompt()
        invocations = [json.loads(line) for line in self.log.read_text().splitlines()]
        completions = [call for call in invocations if call and call[0] == "__complete"]
        executions = [call for call in invocations if not call or call[0] != "__complete"]
        if not completions or len(executions) != 1:
            raise AssertionError(f"{self.kind}: calls={invocations!r}, screen={screen!r}")
        return completions[-1], executions[0]

    def close(self):
        os.write(self.fd, b"exit\n")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            pid, _ = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                os.close(self.fd)
                return
            time.sleep(0.02)
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)
        os.close(self.fd)


class CompletionScripts(unittest.TestCase):
    def check_shell(self, kind, autoload=False):
        if not shutil.which(kind):
            self.skipTest(f"{kind} is not installed")
        with tempfile.TemporaryDirectory(prefix="fwm-completions-") as directory:
            shell = Shell(kind, Path(directory), autoload)
            try:
                call, result = shell.complete("fwm stop ", ["web-prod"])
                self.assertEqual(result, ["stop", "web-prod"])
                self.assertEqual(call[1:3], ["2", "--"])
                self.assertEqual(call[-1], "")

                _, result = shell.complete("fwm add web --server=pr", ["--server=prod"])
                self.assertEqual(result, ["add", "web", "--server=prod"])
                _, result = shell.complete("fwm add web --server=", ["--server=prod"])
                self.assertEqual(result, ["add", "web", "--server=prod"])

                path = str(Path(directory) / "config folder" / "fwm.json")
                _, result = shell.complete("fwm --config " + str(Path(directory) / "con"), [path])
                self.assertEqual(result, ["--config", path])
                _, result = shell.complete("fwm --config=" + str(Path(directory) / "con"), ["--config=" + path])
                self.assertEqual(result, ["--config=" + path])

                Path(path).parent.mkdir()
                _, result = shell.complete("fwm --config " + str(Path(directory) / "con"),
                                           [str(Path(path).parent) + "/"], after_tab="fwm.json")
                self.assertEqual(result, ["--config", path])
                quoted_prefix = shlex.quote(str(Path(directory) / "config f"))
                call, result = shell.complete("fwm --config " + quoted_prefix, [path])
                self.assertEqual(result, ["--config", path])
                self.assertEqual(call[-1], str(Path(directory) / "config f"))

                _, result = shell.complete("fwm stop prefix:pa", ["prefix:path"])
                self.assertEqual(result, ["stop", "prefix:path"])

                marker = Path(directory) / "SHOULD_NOT_EXIST"
                candidate = "$(touch SHOULD_NOT_EXIST);`touch SHOULD_NOT_EXIST`"
                _, result = shell.complete("fwm stop ", [candidate])
                self.assertEqual(result, ["stop", candidate])
                self.assertFalse(marker.exists())

                line = "fwm stop we --json"
                call, result = shell.complete(line, ["web-prod"], cursor=len("fwm stop we"))
                self.assertEqual(result, ["stop", "web-prod", "--json"])
                self.assertEqual(call[1:3], ["2", "--"])

                line = "fwm stop web-prod"
                _, result = shell.complete(line, ["web-prod"], cursor=len("fwm stop we"))
                self.assertEqual(result, ["stop", "web-prod"])

                line = "fwm status we --config-dir profile"
                call, result = shell.complete(line, ["web-prod"], cursor=len("fwm status we"))
                self.assertEqual(call[-2:], ["--config-dir", "profile"])
                self.assertEqual(result, ["status", "web-prod", "--config-dir", "profile"])

                line = "fwm status 生产 --json"
                _, result = shell.complete(line, ["生产转发"], cursor=len("fwm status 生产"))
                self.assertEqual(result, ["status", "生产转发", "--json"])

                command = shlex.quote(str(shell.binary))
                _, result = shell.complete(command + " stop we", ["web-prod"])
                self.assertEqual(result, ["stop", "web-prod"])
            finally:
                shell.close()

    def test_bash(self):
        self.check_shell("bash")

    def test_zsh_source(self):
        self.check_shell("zsh")

    def test_zsh_autoload(self):
        self.check_shell("zsh", autoload=True)

    def test_bash_split_wordbreaks(self):
        if not shutil.which("bash"):
            self.skipTest("bash is not installed")
        with tempfile.TemporaryDirectory(prefix="fwm-completions-") as directory:
            log = Path(directory) / "args"
            binary = Path(directory) / "fwm"
            binary.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > " + shlex.quote(str(log)) +
                              "\nprintf '%s\\n' '--server=prod'\n")
            binary.chmod(0o755)
            program = f"""
source {shlex.quote(str(ROOT / 'completions/fwm.bash'))}
COMP_WORDS=({shlex.quote(str(binary))} add web --server = pr)
COMP_CWORD=5
COMP_LINE={shlex.quote(str(binary) + ' add web --server=pr')}
COMP_POINT=${{#COMP_LINE}}
_fwm fwm pr =
printf '%s\\n' "${{COMPREPLY[@]}}"
"""
            result = subprocess.run(["bash", "--noprofile", "--norc", "-c", program],
                                    check=True, capture_output=True, text=True)
            self.assertEqual(result.stdout, "prod\n")
            self.assertEqual(log.read_text().splitlines(),
                             ["__complete", "3", "--", str(binary), "add", "web", "--server=pr"])


if __name__ == "__main__":
    unittest.main()
