#!/usr/bin/env python3
"""Release installer regressions using local archives and a mocked HTTPS client."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
INSTALLER = REPO / "install.sh"


class ReleaseInstall(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fwm-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.root = self.directory / "install root"
        self.bin = self.directory / "tools"
        self.bin.mkdir()
        self.calls = self.directory / "curl.jsonl"
        self.daemon = self.directory / "daemon.jsonl"
        self.rc = self.directory / "bashrc"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        SHELL="/bin/bash", TMPDIR=str(self.directory),
                        FWM_RELEASE_FIXTURE=str(self.directory), FWM_TEST_OS="Linux", FWM_TEST_ARCH="x86_64")
        self.tool("uname", 'import os,sys\nprint(os.environ["FWM_TEST_OS" if sys.argv[1]=="-s" else "FWM_TEST_ARCH"])\n')
        self.tool("curl", '''import json,os,pathlib,shutil,sys
root=pathlib.Path(os.environ["FWM_RELEASE_FIXTURE"])
args=sys.argv[1:]; url=args[-1]
assert url.startswith("https://github.com/Golden-Pigeon/fwm/releases/"),url
with (root/"curl.jsonl").open("a") as f: f.write(json.dumps(url)+"\\n")
if url.endswith("/latest"):
    print("https://github.com/Golden-Pigeon/fwm/releases/tag/v0.1.0",end="")
else:
    shutil.copyfile(root/url.rsplit("/",1)[1],args[args.index("--output")+1])
''')
        self.prepare()

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)

    def prepare(self, target="x86_64-unknown-linux-musl", missing=None, symlink=False):
        package = self.directory / ("fwm-" + target)
        if package.exists(): shutil.rmtree(package)
        package.mkdir()
        binary = package / "fwm"
        binary.write_text(f"#!{sys.executable}\n" + '''import os,pathlib,sys
args=sys.argv[1:]
if args==["--version"]: print("fwm 0.1.0")
elif args[:1]==["completions"]: print("# fixture completion")
elif args==["daemon","restart"]:
    with (pathlib.Path(os.environ["FWM_RELEASE_FIXTURE"])/"daemon.jsonl").open("a") as f: f.write("restart\\n")
else: raise SystemExit("unexpected command")
''')
        binary.chmod(0o755)
        for name in ["LICENSE", "THIRD_PARTY_NOTICES.txt", "DEPENDENCY_LICENSES.txt"]:
            (package / name).write_text("license fixture\n")
        (package / "dependency-sources").mkdir()
        (package / "dependency-sources/README.txt").write_text("source fixture\n")
        (package / "scripts").mkdir()
        shutil.copy2(REPO / "scripts/install-shell-completions.sh", package / "scripts/install-shell-completions.sh")
        if missing: (package / missing).unlink()
        if symlink:
            binary.unlink()
            binary.symlink_to("LICENSE")
        archive = self.directory / (package.name + ".tar.gz")
        with tarfile.open(archive, "w:gz") as tar: tar.add(package, arcname=package.name)
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.directory / "SHA256SUMS").write_text(f"{checksum}  {archive.name}\n")

    def install(self, *extra):
        return subprocess.run(["bash", str(INSTALLER), "--root", str(self.root), "--shell", "none", *extra],
                              env=self.env, capture_output=True, text=True)

    def test_latest_is_resolved_once_and_install_includes_notices(self):
        result = self.install("--no-start")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual(len(calls), 3)
        self.assertTrue(calls[0].endswith("/latest"))
        self.assertTrue(all("/download/v0.1.0/" in u for u in calls[1:]))
        self.assertTrue((self.root / "bin/fwm").is_file())
        self.assertTrue((self.root / "share/licenses/fwm/DEPENDENCY_LICENSES.txt").is_file())
        self.assertTrue((self.root / "share/licenses/fwm/dependency-sources/README.txt").is_file())
        self.assertFalse(self.daemon.exists())

    def test_platform_matrix(self):
        for system, arch, target in [
            ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ]:
            with self.subTest(target=target):
                self.env.update(FWM_TEST_OS=system, FWM_TEST_ARCH=arch)
                self.prepare(target)
                result = self.install("--version", "v0.1.0", "--no-start")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_default_restart_and_shell_configuration_are_repeatable(self):
        self.rc.write_text("# existing user configuration\n")
        for _ in range(2):
            result = self.install("--version=v0.1.0", "--shell=bash", "--rc-file", str(self.rc))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.daemon.read_text().splitlines(), ["restart", "restart"])
        self.assertEqual(self.rc.read_text().count("# >>> fwm shell completion >>>"), 1)
        self.assertIn("# existing user configuration", self.rc.read_text())

    def test_bad_checksum_does_not_replace_existing_binary(self):
        (self.root / "bin").mkdir(parents=True)
        old = self.root / "bin/fwm"
        old.write_text("original binary")
        (self.directory / "SHA256SUMS").write_text("0" * 64 + "  fwm-x86_64-unknown-linux-musl.tar.gz\n")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("verification failed", result.stderr)
        self.assertEqual(old.read_text(), "original binary")
        self.assertFalse(self.daemon.exists())

    def test_missing_license_and_symlink_binary_are_rejected(self):
        for missing, symlink in [("DEPENDENCY_LICENSES.txt", False), (None, True)]:
            self.prepare(missing=missing, symlink=symlink)
            result = self.install()
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(self.root.exists())

    def test_version_mismatch_is_rejected(self):
        result = self.install("--version", "v9.9.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version does not match", result.stderr)
        self.assertFalse(self.root.exists())

    def test_unsupported_system_and_invalid_arguments_do_not_download(self):
        for args in [("--version", "../bad"), ("--shell", "fish"), ("--rc-file", "rc"), ("--unknown",)]:
            result = self.install(*args)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(self.calls.exists())
        self.env["FWM_TEST_OS"] = "FreeBSD"
        self.assertNotEqual(self.install().returncode, 0)
        self.assertFalse(self.calls.exists())

    def test_help_has_no_side_effects(self):
        self.assertEqual(self.install("--help").returncode, 0)
        self.assertFalse(self.calls.exists())
        self.assertFalse(self.root.exists())


if __name__ == "__main__":
    unittest.main()
