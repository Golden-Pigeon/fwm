"""Pure protocol/error tests; no real processes, sockets, files or signals."""
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "fwm_remote_protocol_helper", Path(__file__).with_name("remote_helper.py")
)
helper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(helper)


class ProtocolErrorTests(unittest.TestCase):
    def test_overlap_distinguishes_ipv4_wildcards_and_normalizes_mapped_ipv6(self):
        cases = (
            ("0.0.0.0", "::1", False),
            ("127.0.0.1", "::1", False),
            ("127.0.0.1", "::ffff:127.0.0.1", True),
            ("0.0.0.0", "::ffff:127.0.0.1", True),
            ("::ffff:0.0.0.0", "127.0.0.1", True),
            ("127.0.0.1", "::", True),
            ("::", "::1", True),
            ("127.0.0.1", "127.0.0.2", False),
        )
        for left, right, expected in cases:
            with self.subTest(left=left, right=right):
                self.assertEqual(helper.overlaps(left, right), expected)
                self.assertEqual(helper.overlaps(right, left), expected)

    def assert_error(self, code, operation, *arguments):
        with self.assertRaises(helper.LeaseError) as caught:
            operation(*arguments)
        self.assertEqual(caught.exception.code, code)

    def test_ssh_connection_normalizes_addresses_and_rejects_missing_or_invalid_ports(self):
        self.assertEqual(
            helper.ssh_connection("::ffff:127.0.0.1 1 ::1 65535"),
            ["127.0.0.1", 1, "::1", 65535],
        )
        for value in (
            "", "127.0.0.1 1234 127.0.0.1", "127.0.0.1 bad 127.0.0.1 22",
            "127.0.0.1 0 127.0.0.1 22", "127.0.0.1 -1 127.0.0.1 22",
            "127.0.0.1 65536 127.0.0.1 22", "127.0.0.1 1234 127.0.0.1 0",
            "127.0.0.1 1234 127.0.0.1 65536", "127.0.0.1 1234 127.0.0.1 22 extra",
        ):
            with self.subTest(value=value):
                self.assert_error("ownership_mismatch", helper.ssh_connection, value)
        self.assert_error("unsupported", helper.ssh_connection, "hostname 1234 127.0.0.1 22")

    def test_readonly_inspection_maps_missing_tools_timeout_and_denied_exit_without_using_output(self):
        arguments = ["unavailable-lsof", "-F", "pfnT"]
        cases = (
            (FileNotFoundError("missing"), "unsupported"),
            (subprocess.TimeoutExpired(arguments, 5), "unsupported"),
        )
        for exception, code in cases:
            with self.subTest(exception=exception), mock.patch.object(helper.subprocess, "run", side_effect=exception):
                self.assert_error(code, helper.run_readonly, arguments)
        denied = subprocess.CompletedProcess(arguments, 2, stdout="untrusted partial output", stderr="permission denied")
        with mock.patch.object(helper.subprocess, "run", return_value=denied):
            self.assert_error("permission_denied", helper.run_readonly, arguments)
        for returncode in (0, 1):
            result = subprocess.CompletedProcess(arguments, returncode, stdout="socket records", stderr="")
            with mock.patch.object(helper.subprocess, "run", return_value=result):
                self.assertEqual(helper.run_readonly(arguments), "socket records")

    def test_nonobject_requests_and_oversized_line_produce_only_json_and_stop_at_the_limit(self):
        adapter = mock.Mock()
        manager = helper.LeaseHelper(adapter)
        output = io.StringIO()
        errors = io.StringIO()
        # A trailing request must never be interpreted as another command after
        # the oversized line: its remainder cannot be safely framed.
        incoming = "null\n[]\n123\n\"claim\"\n" + "x" * (helper.MAX_LINE + 1) + "\n{}\n"
        with mock.patch.object(helper, "LeaseHelper", return_value=manager), \
                mock.patch.object(helper.sys, "stdin", io.StringIO(incoming)), \
                mock.patch.object(helper.sys, "stdout", output), \
                mock.patch.object(helper.sys, "stderr", errors):
            self.assertEqual(helper.main(), 2)
        replies = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(len(replies), 5)
        self.assertTrue(all(not reply["ok"] and reply["code"] == "invalid_request" for reply in replies))
        self.assertTrue(all(reply["protocol"] == 1 for reply in replies))
        self.assertIn("line limit", replies[-1]["message"])
        self.assertEqual(errors.getvalue(), "")
        self.assertEqual(adapter.mock_calls, [])


if __name__ == "__main__":
    unittest.main()
