#!/usr/bin/env python3
"""Run the real Linux helper's parser regressions on the local C toolchain.

No SSH server, Linux procfs, or remote Python runtime is needed. The C fixture
renames the helper's main and only invokes parsers and a temporary registry.
Run with: python3 tests/native_helper.py
"""

import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent


def host_compiler():
    """Keep Apple's compiler, linker, and SDK from the same selected toolchain."""
    environment = os.environ.copy()
    if sys.platform == "darwin":
        compiler = subprocess.check_output(
            ["xcrun", "--sdk", "macosx", "--find", "clang"], text=True, timeout=30
        ).strip()
        sdk = subprocess.check_output(
            ["xcrun", "--sdk", "macosx", "--show-sdk-path"], text=True, timeout=30
        ).strip()
        environment["SDKROOT"] = sdk
        return [compiler, "-isysroot", sdk], environment
    compiler = shlex.split(environment.get("CC", "cc"))
    if not compiler or not shutil.which(compiler[0]):
        raise RuntimeError("a local C compiler is required for native helper tests")
    return compiler, environment


@unittest.skipUnless(
    sys.platform.startswith("linux") or sys.platform == "darwin",
    "Linux helper source tests need a POSIX host compiler; Windows is not supported",
)
class NativeHelperTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build_directory = tempfile.TemporaryDirectory(prefix="fwm-native-unit-")
        cls.addClassCleanup(cls.build_directory.cleanup)
        cls.binary = Path(cls.build_directory.name) / "native_helper_unit"
        compiler, environment = host_compiler()
        result = subprocess.run(
            compiler + [
                "-std=c11", "-O1", "-g", "-Wall", "-Wextra",
                str(ROOT / "tests" / "native_helper_unit.c"), "-o", str(cls.binary),
            ],
            env=environment, text=True, capture_output=True, timeout=60,
        )
        if result.returncode:
            raise RuntimeError("native helper fixture did not compile:\n" + result.stdout + result.stderr)

    def run_case(self, name):
        with tempfile.TemporaryDirectory(prefix="fwm-native-case-") as directory:
            environment = os.environ.copy()
            environment["FWM_TEST_DIRECTORY"] = directory
            result = subprocess.run(
                [str(self.binary), name], env=environment, text=True,
                capture_output=True, timeout=10,
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_tcp4_rows(self):
        self.run_case("tcp4_rows")

    def test_ssh_connection(self):
        self.run_case("ssh_connection")

    def test_native_identity(self):
        self.run_case("native_identity")

    def test_python_identity(self):
        self.run_case("python_identity")

    def test_invalid_identity(self):
        self.run_case("invalid_identity")

    def test_claim_record_roundtrip(self):
        self.run_case("claim_record_roundtrip")

    def test_flat_proofs(self):
        self.run_case("flat_proofs")

    def test_python_proofs(self):
        self.run_case("python_proofs")

    def test_invalid_proofs(self):
        self.run_case("invalid_proofs")

    def test_registry_paths(self):
        self.run_case("registry_paths")


if __name__ == "__main__":
    unittest.main(verbosity=2)
