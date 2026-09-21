#!/usr/bin/env python3
"""Check source installation, daemon restart and real generated completion.

Cargo installs a proxy that delegates script generation and completion requests
unchanged to the built fwm binary. Only ordinary command execution is intercepted.
Daemon calls are recorded without starting a real daemon. All installation roots,
saved profiles and startup files are temporary; HOME is never replaced. Build
first with `cargo build --locked -p fwm`.
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

from shell_completion_scripts import FWM_BINARY, write_config, write_proxy


REPO = Path(__file__).resolve().parents[1]
INSTALLER = REPO / "install-from-source.sh"
PROMPT = b"FWM_INSTALL_READY> "


class InteractiveShell:
    def __init__(self, kind, fixture):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(fixture.directory)
            os.environ.update(fixture.env)
            os.environ["TERM"] = "dumb"
            executable = shutil.which(kind)
            arguments = ([executable, "--noprofile", "--norc", "-i"]
                         if kind == "bash" else [executable, "-dfi"])
            os.execv(executable, arguments)
        setup = f"source {shlex.quote(str(fixture.rc))}; PS1='FWM_INSTALL_''READY> '; "
        setup += "bind 'set bell-style none'" if kind == "bash" else "unsetopt beep"
        os.write(self.fd, (setup + "\n").encode())
        self.read_prompt()

    def read_prompt(self):
        output = b""
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.fd], [], [], max(0, deadline - time.monotonic()))
            if ready:
                output += os.read(self.fd, 65536)
                clean = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", output)
                if clean.endswith(PROMPT):
                    return output
        raise AssertionError(f"Shell did not return to its prompt: {output!r}")

    def complete_status(self):
        os.write(self.fd, b"fwm --config-dir profile status ori\t\n")
        return self.read_prompt()

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


class InstallFromSource(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not FWM_BINARY.is_file():
            raise RuntimeError("Build the test binary first: cargo build --locked -p fwm")

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="fwm-source-install-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name).resolve()
        self.root = self.directory / "install root"
        self.rc = self.directory / "shell rc"
        self.log = self.directory / "fwm-calls.jsonl"
        self.daemon_log = self.directory / "daemon-calls.jsonl"
        self.cargo_log = self.directory / "cargo-calls.jsonl"
        self.fake_bin = self.directory / "fake-bin"
        self.fake_bin.mkdir()
        fake_fwm = self.directory / "fake-fwm"
        write_proxy(fake_fwm)
        with fake_fwm.open("a") as stream:
            stream.write(
                "if args == ['daemon', 'restart']:\n"
                "    with open(os.environ['FWM_INSTALL_TEST_DAEMON_LOG'], 'a') as stream:\n"
                "        stream.write(json.dumps(os.path.realpath(sys.argv[0])) + '\\n')\n"
                "    sys.exit(int(os.environ.get('FWM_INSTALL_TEST_FAIL_DAEMON', '0')))\n"
            )
        write_config(self.directory / "profile", names=("orient",))
        cargo = self.fake_bin / "cargo"
        cargo.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, shutil, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['FWM_INSTALL_TEST_CARGO_LOG'], 'a') as stream:\n"
            "    stream.write(json.dumps(args) + '\\n')\n"
            "if os.environ.get('FWM_INSTALL_TEST_FAIL_CARGO'):\n"
            "    sys.exit(23)\n"
            "root = next((arg.split('=', 1)[1] for arg in args if arg.startswith('--root=')), None)\n"
            "if root is None:\n"
            "    root = args[args.index('--root') + 1]\n"
            "bindir = pathlib.Path(root) / 'bin'\n"
            "bindir.mkdir(parents=True, exist_ok=True)\n"
            "shutil.copy2(os.environ['FWM_INSTALL_TEST_BINARY'], bindir / 'fwm')\n"
        )
        cargo.chmod(0o755)
        self.env = dict(os.environ)
        self.env.update({
            "PATH": str(self.fake_bin) + os.pathsep + os.environ.get("PATH", ""),
            "FWM_COMPLETION_LOG": str(self.log),
            "FWM_INSTALL_TEST_DAEMON_LOG": str(self.daemon_log),
            "FWM_INSTALL_TEST_CARGO_LOG": str(self.cargo_log),
            "FWM_INSTALL_TEST_BINARY": str(fake_fwm),
            # If compinit writes a dump, keep that dump inside this fixture.
            "ZDOTDIR": str(self.directory),
        })

    def install(self, kind="bash", extra=(), check=True):
        args = ["bash", str(INSTALLER), "--root", str(self.root), "--shell", kind]
        if kind != "none":
            args += ["--rc-file", str(self.rc)]
        result = subprocess.run(
            [*args, *extra],
            cwd=self.directory, env=self.env, capture_output=True, text=True,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def daemon_calls(self):
        return [json.loads(line) for line in self.daemon_log.read_text().splitlines()] if self.daemon_log.exists() else []

    def run_shell(self, kind, program):
        executable = shutil.which(kind)
        if not executable:
            self.skipTest(f"{kind} is not installed")
        args = ([executable, "--noprofile", "--norc", "-c", program]
                if kind == "bash" else [executable, "-dfc", program])
        result = subprocess.run(args, cwd=self.directory, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def assert_installed_files(self):
        for kind, installed in [
            ("zsh", "share/zsh/site-functions/_fwm"),
            ("bash", "share/bash-completion/completions/fwm"),
        ]:
            expected = subprocess.run(
                [str(FWM_BINARY), "completions", kind], check=True,
                capture_output=True, text=True,
            ).stdout
            self.assertEqual((self.root / installed).read_text(), expected)
        for kind in ["bash", "zsh"]:
            self.assertTrue((self.root / f"share/fwm/shell-init.{kind}").is_file())

    def test_installs_completion_scripts_then_restarts_installed_daemon(self):
        self.install(extra=["--offline"])
        self.assert_installed_files()
        self.assertEqual(self.calls(), [
            {"args": ["completions", "zsh"], "complete": None},
            {"args": ["completions", "bash"], "complete": None},
            {"args": ["daemon", "restart"], "complete": None},
        ])
        self.assertEqual(self.daemon_calls(), [str(self.root / "bin/fwm")])
        cargo_args = json.loads(self.cargo_log.read_text().splitlines()[0])
        for argument in ["install", "--locked", "--force", "--offline"]:
            self.assertIn(argument, cargo_args)

    def test_reinstall_preserves_rc_and_backs_up_original(self):
        # Deliberately omit a final newline: appending a marker must not join it.
        original = "# user configuration\nexport FWM_INSTALL_KEEP='retained'"
        self.rc.write_text(original)
        self.install()
        first = self.rc.read_text()
        self.assertIn(original + "\n", first)
        backups = [path for path in self.directory.glob(self.rc.name + "*") if path != self.rc]
        self.assertTrue(any(path.is_file() and path.read_text() == original for path in backups), backups)
        self.install()
        self.assertEqual(self.rc.read_text(), first)
        self.assertEqual(self.daemon_calls(), [str(self.root / "bin/fwm")] * 2)
        self.assertEqual(first.count("# >>> fwm shell completion >>>"), 1)
        result = self.run_shell("bash", f"source {shlex.quote(str(self.rc))}; printf '%s' \"$FWM_INSTALL_KEEP\"")
        self.assertEqual(result.stdout, "retained")

    def test_reinstall_updates_existing_block(self):
        self.rc.write_text("# before installer\n")
        self.install()
        self.rc.write_text(self.rc.read_text() + "# after installer\n")
        self.root = self.directory / "replacement root"
        self.install()
        text = self.rc.read_text()
        self.assertIn("# before installer\n", text)
        self.assertIn("# after installer\n", text)
        self.assertEqual(text.count("# >>> fwm shell completion >>>"), 1)
        result = self.run_shell("bash", f"source {shlex.quote(str(self.rc))}; command -v fwm")
        self.assertEqual(result.stdout.strip(), str(self.root / "bin/fwm"))

    def test_cargo_failure_does_not_change_startup(self):
        original = "# preserve me\n"
        self.rc.write_text(original)
        self.env["FWM_INSTALL_TEST_FAIL_CARGO"] = "1"
        result = self.install(check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.rc.read_text(), original)
        self.assertFalse(self.log.exists())
        self.assertEqual(self.daemon_calls(), [])
        self.assertFalse((self.root / "share").exists())
        self.assertNotIn("Installed fwm from source.", result.stdout)

    def test_completion_setup_failure_does_not_restart_daemon(self):
        self.root.mkdir()
        (self.root / "share").write_text("blocks completion installation\n")
        result = self.install(check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.root / "bin/fwm").is_file())
        self.assertEqual(self.daemon_calls(), [])
        self.assertNotIn("Installed fwm from source.", result.stdout)

    def test_daemon_failure_preserves_exit_code_and_reports_installed_binary(self):
        self.env["FWM_INSTALL_TEST_FAIL_DAEMON"] = "41"
        result = self.install(check=False)
        self.assertEqual(result.returncode, 41, result.stdout + result.stderr)
        self.assertEqual(self.daemon_calls(), [str(self.root / "bin/fwm")])
        self.assertTrue((self.root / "bin/fwm").is_file())
        self.assert_installed_files()
        self.assertIn("installed", result.stderr.lower())
        self.assertIn("daemon startup/restart failed", result.stderr.lower())
        retry = next(line.removeprefix("Retry with: ") for line in result.stderr.splitlines()
                     if line.startswith("Retry with: "))
        self.assertEqual(shlex.split(retry), [str(self.root / "bin/fwm"), "daemon", "restart"])
        self.assertNotIn("Installed fwm from source.", result.stdout)

    def test_none_leaves_startup_file_untouched(self):
        original = "# do not automatically enable completion\n"
        self.rc.write_text(original)
        self.install(kind="none")
        self.assertEqual(self.rc.read_text(), original)
        self.assert_installed_files()
        self.assertFalse((self.directory / ".zshrc").exists())
        self.assertEqual(self.daemon_calls(), [str(self.root / "bin/fwm")])

    def test_none_with_rc_override_is_rejected_before_cargo(self):
        result = self.install(kind="none", extra=["--rc-file", str(self.rc)], check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.cargo_log.exists())
        self.assertFalse(self.rc.exists())

    def test_help_works_when_shell_is_unset(self):
        result = subprocess.run(
            ["bash", "-c", 'unset SHELL; source "$@"', "fwm-install-test", str(INSTALLER), "--help"],
            cwd=self.directory, env=self.env, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Usage:", result.stdout)
        self.assertFalse(self.cargo_log.exists())
        self.assertEqual(self.daemon_calls(), [])

    def test_unset_or_unknown_shell_installs_files_without_startup(self):
        for shell in [None, "/not-installed/fish"]:
            with self.subTest(shell=shell):
                args = [str(INSTALLER), "--root", str(self.root)]
                env = dict(self.env)
                if shell is None:
                    # Bash may recreate SHELL from passwd at process startup.
                    # Unset it inside Bash to exercise a genuinely absent value.
                    command = ["bash", "-c", 'unset SHELL; source "$@"', "fwm-install-test", *args]
                else:
                    env["SHELL"] = shell
                    command = ["bash", *args]
                result = subprocess.run(command, cwd=self.directory, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_installed_files()
                self.assertFalse(self.rc.exists())
                self.assertFalse((self.directory / ".zshrc").exists())
                self.assertIn("Startup files unchanged", result.stdout)

    def test_explicit_unknown_shell_is_rejected_before_cargo(self):
        result = self.install(kind="unsupported", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.cargo_log.exists())
        self.assertFalse(self.rc.exists())

    def test_startup_symlink_is_preserved_and_target_is_backed_up(self):
        target = self.directory / "dotfiles" / "bashrc"
        target.parent.mkdir()
        original = "# managed by my dotfile tool\n"
        target.write_text(original)
        target.chmod(0o640)
        self.rc.symlink_to("dotfiles/bashrc")
        self.install()
        self.assertTrue(self.rc.is_symlink())
        self.assertEqual(os.readlink(self.rc), "dotfiles/bashrc")
        self.assertIn(original, target.read_text())
        self.assertEqual(target.stat().st_mode & 0o777, 0o640)
        backups = list(target.parent.glob("bashrc.fwm-backup.*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_text(), original)
        result = self.run_shell("bash", f"source {shlex.quote(str(self.rc))}; command -v fwm")
        self.assertEqual(result.stdout.strip(), str(self.root / "bin/fwm"))

    def test_malformed_completion_markers_preserve_startup(self):
        begin = "# >>> fwm shell completion >>>\n"
        end = "# <<< fwm shell completion <<<\n"
        for block in [begin, end, begin + begin + end]:
            with self.subTest(block=block):
                original = "# user prefix\n" + block + "export FWM_KEEP=important\n"
                self.rc.write_text(original)
                result = self.install(check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.rc.read_text(), original)
                self.assertIn("startup file was preserved", result.stderr)
                self.assertEqual(list(self.directory.glob(".fwm-shell-rc.*")), [])
                self.assertEqual(list(self.directory.glob(self.rc.name + ".fwm-backup.*")), [])
                self.assertEqual(self.daemon_calls(), [])
                self.assertNotIn("Installed fwm from source.", result.stdout)

    def test_bash_loader_registers_completion_and_deduplicates_path(self):
        self.install()
        source = f"source {shlex.quote(str(self.rc))}"
        result = self.run_shell("bash", source + "; " + source +
                                "; complete -p fwm; command -v fwm; printf '%s\\n' \"$PATH\"")
        lines = result.stdout.splitlines()
        self.assertIn("-F _clap_complete_fwm fwm", lines[0])
        self.assertEqual(lines[1], str(self.root / "bin/fwm"))
        self.assertEqual(lines[2].split(os.pathsep).count(str(self.root / "bin")), 1)

    def test_loaders_prioritize_installation_already_late_in_path(self):
        stale_fwm = self.fake_bin / "fwm"
        stale_fwm.write_text("#!/bin/sh\nexit 92\n")
        stale_fwm.chmod(0o755)
        self.env["PATH"] += os.pathsep + str(self.root / "bin") + os.pathsep + str(self.root / "bin")
        for kind in ["bash", "zsh"]:
            if not shutil.which(kind):
                continue
            with self.subTest(shell=kind):
                self.install(kind=kind)
                result = self.run_shell(kind, "command -v fwm; "
                                        f"source {shlex.quote(str(self.rc))}; "
                                        "command -v fwm; printf '%s\\n' \"$PATH\"")
                before, after, path = result.stdout.splitlines()
                self.assertEqual(before, str(stale_fwm))
                self.assertEqual(after, str(self.root / "bin/fwm"))
                self.assertEqual(path.split(os.pathsep)[0], str(self.root / "bin"))
                self.assertEqual(path.split(os.pathsep).count(str(self.root / "bin")), 1)

    def test_install_and_reinstall_restart_fresh_binary_despite_stale_path(self):
        stale_fwm = self.fake_bin / "fwm"
        stale_fwm.write_text("#!/bin/sh\nexit 92\n")
        stale_fwm.chmod(0o755)
        self.env["PATH"] += os.pathsep + str(self.root / "bin")
        self.install(kind="none")
        # Cargo must replace an old executable at the destination on reinstall.
        shutil.copy2(stale_fwm, self.root / "bin/fwm")
        self.install(kind="none")
        self.assertEqual(self.daemon_calls(), [str(self.root / "bin/fwm")] * 2)
        self.assertEqual([call["args"] for call in self.calls()],
                         [["completions", "zsh"], ["completions", "bash"],
                          ["daemon", "restart"]] * 2)

    def test_zsh_loader_initializes_completion_when_needed(self):
        self.install(kind="zsh")
        result = self.run_shell("zsh", f"source {shlex.quote(str(self.rc))}; "
                                "print -r -- ${_comps[fwm]}; whence -p fwm")
        self.assertEqual(result.stdout.splitlines(), ["_clap_dynamic_completer_fwm", str(self.root / "bin/fwm")])

    def test_zsh_loader_reuses_existing_completion_framework(self):
        self.install(kind="zsh")
        result = self.run_shell("zsh", "autoload -Uz compinit; compinit -D; "
                                "compinit() { print 'UNEXPECTED_COMPINIT'; return 42; }; "
                                f"source {shlex.quote(str(self.rc))}; "
                                f"source {shlex.quote(str(self.rc))}; "
                                "print -r -- ${_comps[fwm]}; print -r -- $PATH")
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], "_clap_dynamic_completer_fwm")
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[1].split(os.pathsep).count(str(self.root / "bin")), 1)

    def test_shell_metacharacters_in_paths_are_literal(self):
        for kind in ["bash", "zsh"]:
            if not shutil.which(kind):
                continue
            with self.subTest(shell=kind):
                self.root = self.directory / (kind + " space ' $(touch BAD_DOLLAR) `touch BAD_BACKTICK`")
                self.rc = self.directory / (kind + " rc ' $(touch BAD_RC)")
                self.install(kind=kind)
                result = self.run_shell(kind, f"source {shlex.quote(str(self.rc))}; command -v fwm")
                self.assertEqual(result.stdout.strip(), str(self.root / "bin/fwm"))
                for marker in ["BAD_DOLLAR", "BAD_BACKTICK", "BAD_RC"]:
                    self.assertFalse((self.directory / marker).exists())

    def check_interactive_completion(self, kind):
        if not shutil.which(kind):
            self.skipTest(f"{kind} is not installed")
        self.install(kind=kind)
        self.log.write_text("")
        shell = InteractiveShell(kind, self)
        try:
            screen = shell.complete_status()
            calls = self.calls()
            self.assertTrue(any(call["complete"] == kind for call in calls), screen)
            self.assertEqual([call["args"] for call in calls
                              if not call["complete"] and call["args"][:1] != ["completions"]],
                             [["--config-dir", "profile", "status", "orient"]], screen)
            self.assertEqual(sorted(path.name for path in (self.directory / "profile").iterdir()),
                             ["config.toml"])
        finally:
            shell.close()

    def test_bash_dynamic_completion_after_install(self):
        self.check_interactive_completion("bash")

    def test_zsh_dynamic_completion_after_install(self):
        self.check_interactive_completion("zsh")


if __name__ == "__main__":
    unittest.main()
