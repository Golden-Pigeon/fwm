#!/usr/bin/env python3
"""Exercise clap_complete's generated scripts with real line editors (no daemon).

Only normal command execution is intercepted. Completion requests are delegated
unchanged to the built fwm binary and read saved configuration from a temporary
profile. Build first with `cargo build --locked -p fwm`.
"""

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
FWM_BINARY = Path(os.environ.get("FWM_TEST_BINARY", ROOT / "target/debug/fwm")).resolve()
PROMPT = b"FWM_COMPLETION_READY> "


def upstream_bash_expected_failure(test):
    # Reproduced with pinned clap_complete 4.6.11 on Bash 3.2 and Linux Bash 5.2.
    # An upstream fix must produce an unexpected success so this is revisited.
    return unittest.expectedFailure(test)


def write_config(directory, names=("web-prod", "orient", "生产转发")):
    """Write an offline profile without invoking configuration or daemon commands."""
    directory.mkdir(parents=True, exist_ok=True)
    text = '''schema_version = 3
[[servers]]
id = "prod"
name = "prod"
host = "127.0.0.1"
'''
    records = [(name, "rule-" + str(index)) for index, name in enumerate(names)]
    records += [("colon-id", "prefix:path"),
                ("special-id", "literal$(touch SHOULD_NOT_EXIST);`touch SHOULD_NOT_EXIST`")]
    for index, (name, identity) in enumerate(records):
        text += f'''
[[forwards]]
id = {json.dumps(identity)}
name = {json.dumps(name, ensure_ascii=False)}
group = "apps"
server_id = "prod"
kind = "local"
listen = "127.0.0.1:{31001 + index}"
target = "localhost:8080"
desired_state = "stopped"
'''
    (directory / "config.toml").write_text(text)


def write_proxy(path):
    """Delegate the official completion protocol; never execute ordinary commands."""
    path.write_text(
        f"#!{sys.executable}\n"
        "import json, os, sys\n"
        "args = sys.argv[1:]\n"
        "mode = os.environ.get('FWM_COMPLETE')\n"
        "with open(os.environ['FWM_COMPLETION_LOG'], 'a') as stream:\n"
        "    stream.write(json.dumps({'args': args, 'complete': mode}) + '\\n')\n"
        "if mode or args[:1] == ['completions']:\n"
        f"    os.execv({str(FWM_BINARY)!r}, [{str(FWM_BINARY)!r}, *args])\n"
    )
    path.chmod(0o755)


class Shell:
    def __init__(self, kind, directory, autoload=False):
        self.kind = kind
        self.directory = directory
        self.log = directory / "invocations.jsonl"
        write_config(directory / "profile")
        self.initial_config = (directory / "profile/config.toml").read_bytes()
        bindir = directory / "bin"
        bindir.mkdir()
        self.binary = bindir / "fwm"
        write_proxy(self.binary)
        registration = subprocess.run(
            [str(FWM_BINARY), "completions", kind], check=True,
            capture_output=True, text=True,
        ).stdout
        completions = directory / "completions"
        completions.mkdir()
        script = completions / ("_fwm" if kind == "zsh" else "fwm")
        script.write_text(registration)
        setup = directory / "setup"
        lines = [
            f"export PATH={shlex.quote(str(bindir))}:$PATH",
            f"export FWM_COMPLETION_LOG={shlex.quote(str(self.log))}",
            f"export ZDOTDIR={shlex.quote(str(directory))}",
            "PS1='FWM_COMPLETION_READY> '",
        ]
        if kind == "bash":
            lines += [f"source {shlex.quote(str(script))}", "bind 'set bell-style none'"]
        else:
            if autoload:
                lines += [f"fpath=({shlex.quote(str(completions))} $fpath)"]
            lines += ["autoload -Uz compinit; compinit -D", "unsetopt beep"]
            if not autoload:
                lines += [f"source {shlex.quote(str(script))}"]
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

    def complete(self, line, cursor=None, after_tab="", allow_bash_syntax_error=False):
        self.log.write_text("")
        keys = line.encode()
        if cursor is not None:
            keys += b"\x1b[D" * (len(line) - cursor)
        os.write(self.fd, keys + b"\t" + after_tab.encode() + b"\n")
        screen = self.read_prompt()
        invocations = [json.loads(line) for line in self.log.read_text().splitlines()]
        completions = [call["args"] for call in invocations if call["complete"]]
        executions = [call["args"] for call in invocations if not call["complete"]]
        known_syntax_error = (allow_bash_syntax_error and self.kind == "bash"
                              and not executions and b"syntax error near unexpected token" in screen)
        if not completions or (len(executions) != 1 and not known_syntax_error):
            raise AssertionError(f"{self.kind}: calls={invocations!r}, screen={screen!r}")
        config = self.directory / "profile"
        if sorted(path.name for path in config.iterdir()) != ["config.toml"]:
            raise AssertionError("completion created state or daemon files")
        if (config / "config.toml").read_bytes() != self.initial_config:
            raise AssertionError("completion modified saved configuration")
        return completions[-1], executions[0] if executions else None

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
        # Close the PTY master before waiting: on macOS the child can remain
        # in terminal teardown while unread output is held by this master.
        os.close(self.fd)
        os.waitpid(self.pid, 0)


class CompletionScripts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not FWM_BINARY.is_file():
            raise RuntimeError("Build the test binary first: cargo build --locked -p fwm")

    def check_shell(self, kind, check, autoload=False):
        if not shutil.which(kind):
            self.skipTest(f"{kind} is not installed")
        with tempfile.TemporaryDirectory(prefix="fwm-completions-") as directory:
            shell = Shell(kind, Path(directory), autoload)
            try:
                check(shell)
            finally:
                shell.close()

    def shells(self, check):
        for kind in ["bash", "zsh"]:
            with self.subTest(shell=kind):
                self.check_shell(kind, check)

    def test_saved_rules_servers_groups_and_options(self):
        def check(shell):
            for fragment, expected in [
                ("status we", ["status", "web-prod"]),
                ("server remove pr", ["server", "remove", "prod"]),
                ("add web --server pr", ["add", "web", "--server", "prod"]),
                ("status --group ap", ["status", "--group", "apps"]),
                ("status --ser", ["status", "--server"]),
                ("status 生产", ["status", "生产转发"]),
            ]:
                with self.subTest(fragment=fragment):
                    _, result = shell.complete("fwm --config-dir profile " + fragment)
                    self.assertEqual(result, ["--config-dir", "profile", *expected])
        self.shells(check)

    def test_unknown_rule_does_not_complete_unrelated_files(self):
        def check(shell):
            (shell.directory / "unknown-file").write_text("not a forwarding rule")
            _, result = shell.complete("fwm --config-dir profile status unknown")
            self.assertEqual(result, ["--config-dir", "profile", "status", "unknown"])
        self.shells(check)

    def test_configuration_directory_with_spaces(self):
        def check(shell):
            write_config(shell.directory / "custom profile")
            for directory in ["'custom profile'", r"custom\ profile"]:
                with self.subTest(directory=directory):
                    _, result = shell.complete("fwm --config-dir " + directory + " status we")
                    self.assertEqual(result, ["--config-dir", "custom profile", "status", "web-prod"])
        self.shells(check)

    def check_equals(self, shell):
        for prefix in ["pr", ""]:
            _, result = shell.complete("fwm --config-dir profile add web --server=" + prefix)
            self.assertEqual(result, ["--config-dir", "profile", "add", "web", "--server=prod"])

    def test_zsh_equals_server_value(self):
        self.check_shell("zsh", self.check_equals)

    @upstream_bash_expected_failure
    def test_upstream_bash_equals_value(self):
        # clap_complete 4.6.11 duplicates the --server= prefix with Bash 3.2.
        # Keep this regression visible so an upstream fix prompts its promotion.
        self.check_shell("bash", self.check_equals)

    def test_filesystem_completion(self):
        def check(shell):
            (shell.directory / "fixture_config").write_text("# SSH config")
            _, result = shell.complete("fwm --config-dir profile server add new --ssh-config fixt")
            self.assertEqual(result, ["--config-dir", "profile", "server", "add", "new", "--ssh-config", "fixture_config"])
        self.shells(check)

    def check_directory_spaces(self, shell):
        folder = shell.directory / "config folder"
        folder.mkdir()
        (folder / "fwm.json").write_text("{}")
        _, result = shell.complete("fwm --config-dir profile server add new --ssh-config con", after_tab="fwm.json")
        self.assertEqual(result, ["--config-dir", "profile", "server", "add", "new", "--ssh-config", "config folder/fwm.json"])

    def test_zsh_directory_spaces(self):
        self.check_shell("zsh", self.check_directory_spaces)

    def test_bash_directory_spaces(self):
        # Readline's standard filenames option quotes candidates and preserves
        # directory continuation without a custom shell-escaping adapter.
        self.check_shell("bash", self.check_directory_spaces)

    def check_quoted_path(self, shell):
        folder = shell.directory / "config folder"
        folder.mkdir()
        (folder / "fwm.json").write_text("{}")
        _, result = shell.complete("fwm --config-dir profile server add new --ssh-config 'config folder/f'")
        self.assertEqual(result, ["--config-dir", "profile", "server", "add", "new", "--ssh-config", "config folder/fwm.json"])

    @upstream_bash_expected_failure
    def test_upstream_bash_quoted_path(self):
        # Upstream receives shell quote characters instead of an unquoted path.
        self.check_shell("bash", self.check_quoted_path)

    @unittest.expectedFailure
    def test_upstream_zsh_quoted_path(self):
        self.check_shell("zsh", self.check_quoted_path)

    def check_colon(self, shell):
        _, result = shell.complete("fwm --config-dir profile status prefix:pa")
        self.assertEqual(result, ["--config-dir", "profile", "status", "prefix:path"])

    def test_zsh_colon_in_id(self):
        self.check_shell("zsh", self.check_colon)

    @upstream_bash_expected_failure
    def test_upstream_bash_colon_in_id(self):
        # Bash 3.2's word-break handling duplicates the colon prefix.
        self.check_shell("bash", self.check_colon)

    def test_shell_metacharacters_do_not_execute(self):
        def check(shell):
            _, result = shell.complete("fwm --config-dir profile status literal", allow_bash_syntax_error=True)
            self.assertFalse((shell.directory / "SHOULD_NOT_EXIST").exists(),
                             "completion candidate was executed as shell code")
            if result is not None:
                self.assertEqual(result, ["--config-dir", "profile", "status",
                                         "literal$(touch SHOULD_NOT_EXIST);`touch SHOULD_NOT_EXIST`"])
        self.shells(check)

    def test_completion_before_remaining_arguments(self):
        def check(shell):
            line = "fwm --config-dir profile status we --json"
            _, result = shell.complete(line, cursor=len("fwm --config-dir profile status we"))
            self.assertEqual(result, ["--config-dir", "profile", "status", "web-prod", "--json"])
            line = "fwm status we --config-dir profile"
            call, result = shell.complete(line, cursor=len("fwm status we"))
            self.assertEqual(call[-2:], ["--config-dir", "profile"])
            self.assertEqual(result, ["status", "web-prod", "--config-dir", "profile"])
        self.shells(check)

    def check_middle_word(self, shell):
        line = "fwm --config-dir profile status web-prod"
        _, result = shell.complete(line, cursor=len("fwm --config-dir profile status we"))
        self.assertEqual(result, ["--config-dir", "profile", "status", "web-prod"])

    def test_zsh_cursor_in_middle_of_word(self):
        self.check_shell("zsh", self.check_middle_word)

    @upstream_bash_expected_failure
    def test_upstream_bash_cursor_in_middle_of_word(self):
        # The pinned Bash integration preserves the suffix after the cursor when inserting a match.
        self.check_shell("bash", self.check_middle_word)

    def check_absolute_command(self, shell):
        _, result = shell.complete(shlex.quote(str(shell.binary)) + " --config-dir profile status we")
        self.assertEqual(result, ["--config-dir", "profile", "status", "web-prod"])

    def test_absolute_command_path(self):
        self.shells(self.check_absolute_command)

    @unittest.expectedFailure
    def test_upstream_zsh_fpath_autoload_first_tab(self):
        # Dynamic registration is intended to be sourced at startup. A bare
        # fpath autoload only registers its function on the first Tab; the
        # installer therefore explicitly sources the generated registration.
        self.check_shell("zsh", self.check_absolute_command, autoload=True)


if __name__ == "__main__":
    import faulthandler
    faulthandler.dump_traceback_later(180, exit=True)
    unittest.main()
