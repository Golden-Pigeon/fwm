#!/usr/bin/env python3
"""Exercise native launchd upgrades using only a temporary fwm profile/service.

Requires macOS with a logged-in GUI user. Registers a distinct test LaunchAgent
in that user's LaunchAgents directory and removes it on exit.
"""
import argparse
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time


def run(binary, old_binary):
    with tempfile.TemporaryDirectory(prefix="fwm-launchd-upgrade-") as directory:
        root = Path(directory).resolve()
        profile = root / "profile"
        executable = root / "install/bin/fwm"
        executable.parent.mkdir(parents=True)
        previous = root / "previous-fwm"
        shutil.copy2(old_binary, previous)
        shutil.copy2(previous, executable)
        value = 0xCBF29CE484222325
        for byte in os.fsencode(profile):
            value = ((value ^ byte) * 0x100000001B3) & ((1 << 64) - 1)
        label = f"io.fwm.fwm-{value:016x}"
        domain = f"gui/{os.getuid()}"
        service = f"{domain}/{label}"
        plist = Path.home() / "Library/LaunchAgents" / f"{label}.plist"
        assert not plist.exists(), "test service unexpectedly exists"

        def cli(*args):
            result = subprocess.run(
                [str(executable), "--config-dir", str(profile), *args],
                capture_output=True, text=True, timeout=45,
            )
            assert result.returncode == 0, (
                f"{args}: exit {result.returncode}\n{result.stdout}\n{result.stderr}"
            )
            return result.stdout

        def loaded():
            result = subprocess.run(
                ["launchctl", "print", service],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0:
                return result.stdout
            assert "Could not find service" in result.stderr, result.stderr
            return None

        def replace_binary():
            # Match the installer's atomic executable replacement.
            candidate = executable.with_suffix(".new")
            shutil.copy2(binary, candidate)
            candidate.replace(executable)

        try:
            cli("service", "install")
            replace_binary()
            for _ in range(3):
                before = loaded()
                pid = re.search(r"\bpid = (\d+)", before).group(1)
                cli("daemon", "restart")
                state = json.loads(cli("service", "status", "--json"))
                assert state["registered"] and state["running"] and state["enabled"], state
                assert re.search(r"\bpid = (\d+)", loaded()).group(1) != pid
            print("PASS native managed upgrade and repeated restarts", flush=True)

            # Keep the launchd job alive briefly after the child daemon drops its
            # instance lock. The old lifecycle raced bootstrap against this job.
            cli("daemon", "stop")
            wrapper = root / "delayed-exit"
            wrapper.write_text(
                f"#!{sys.executable}\n"
                "import signal, subprocess, sys, time\n"
                f"child = subprocess.Popen([{str(previous)!r}, *sys.argv[1:]])\n"
                "def stop(*_):\n"
                "    child.terminate()\n"
                "    child.wait()\n"
                "    time.sleep(2)\n"
                "    sys.exit(0)\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                "child.wait()\n"
            )
            wrapper.chmod(0o700)
            original = plist.read_text()
            needle = f"<string>{html.escape(str(executable))}</string>"
            assert needle in original
            plist.write_text(original.replace(needle, f"<string>{html.escape(str(wrapper))}</string>"))
            subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True, timeout=10)
            deadline = time.monotonic() + 10
            while not json.loads(cli("daemon", "status", "--json"))["daemon_running"]:
                assert time.monotonic() < deadline, "test daemon did not start"
                time.sleep(0.05)
            assert f"program = {wrapper}" in loaded()
            replace_binary()
            cli("daemon", "restart")
            assert f"program = {executable}\n" in loaded()
            assert json.loads(cli("daemon", "status", "--json"))["daemon_running"]
            print("PASS upgrade waits for a delayed launchd job to disappear", flush=True)
        finally:
            # Exercise normal cleanup, but retain a bounded fallback so an
            # unfixed binary can be tested without leaking its temporary job.
            try:
                cli("service", "uninstall")
            finally:
                if loaded() is not None:
                    subprocess.run(["launchctl", "bootout", service], capture_output=True, timeout=15)
                deadline = time.monotonic() + 15
                while loaded() is not None:
                    assert time.monotonic() < deadline, "test service cleanup did not finish"
                    time.sleep(0.05)
                if plist.exists():
                    assert str(profile) in plist.read_text(), "test definition was replaced externally"
                    plist.unlink()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--old-binary", type=Path, help="optional previous release to upgrade from")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("this test requires native macOS launchd")
    run(args.binary.resolve(), (args.old_binary or args.binary).resolve())
