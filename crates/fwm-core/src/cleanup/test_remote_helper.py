"""Remote lease tests. Every signal target is an isolated test subprocess.

Run: python3 -m unittest discover -s crates/fwm-core/src/cleanup -p 'test_*.py'
"""

import contextlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import uuid


SCRIPT = Path(__file__).with_name("remote_helper.py")
SPEC = importlib.util.spec_from_file_location("fwm_remote_helper", SCRIPT)
helper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(helper)


def command(generation=1, **overrides):
    result = dict(op="claim", owner_id=str(uuid.uuid4()), rule_id=str(uuid.uuid4()),
                  session_id=str(uuid.uuid4()), generation=generation,
                  listen_host="127.0.0.1", listen_port=12345)
    result.update(overrides)
    return result


def process(pid, parent, name="sshd", birth=None):
    return dict(pid=pid, ppid=parent, name=name, uid=os.geteuid(), birth=birth or str(pid), state="S")


class FakePlatform:
    def __init__(self):
        self.processes = {100: process(100, 1), 101: process(101, 100, "python3"),
                          200: process(200, 1), 201: process(201, 200, "python3")}
        self.tcp = []
        self.signals = []

    def process(self, pid):
        return self.processes.get(pid)

    def sockets(self):
        return self.tcp

    def owns(self, pid, connection):
        return connection["pid"] == pid

    def open_process(self, identity):
        return identity["pid"]

    def signal_process(self, descriptor, identity, sig):
        self.signals.append((descriptor, sig))
        self.processes.pop(descriptor, None)
        self.tcp = [item for item in self.tcp if item["pid"] != descriptor]

    def close_process(self, descriptor):
        pass

    def connected(self, pid, source_port):
        self.tcp.append(dict(pid=pid, local=("127.0.0.1", 22), remote=("127.0.0.1", source_port), listening=False))
        return ["127.0.0.1", source_port, "127.0.0.1", 22]

    def listen(self, pid, port):
        self.tcp.append(dict(pid=pid, local=("127.0.0.1", port), remote=("0.0.0.0", 0), listening=True))


class RegistryTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.environment = mock.patch.dict(os.environ, FWM_REMOTE_STATE_DIR=self.directory.name)
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.bindable = mock.patch.object(helper, "port_available", return_value=True)
        self.bindable.start()
        self.addCleanup(self.bindable.stop)
        self.adapter = FakePlatform()
        self.old_transport = self.adapter.connected(100, 40001)
        self.new_transport = self.adapter.connected(200, 40002)

    def manager(self, pid=100):
        instance = helper.LeaseHelper(self.adapter)
        transport = self.old_transport if pid == 100 else self.new_transport
        instance.identify = lambda: (self.adapter.process(pid), self.adapter.process(pid + 1), transport)
        return instance

    def assert_error(self, code, operation, *args):
        with self.assertRaises(helper.LeaseError) as caught:
            operation(*args)
        self.assertEqual(caught.exception.code, code)

    def test_claim_confirm_and_release_are_generation_fenced(self):
        old = self.manager()
        request = command()
        self.assertFalse(old.claim(request)["reclaimed"])
        self.assert_error("listener_missing", old.confirm)
        self.adapter.listen(100, request["listen_port"])
        self.assertTrue(old.confirm()["ok"])
        replacement = dict(request, generation=2, session_id=str(uuid.uuid4()))
        new = self.manager(200)
        self.assertTrue(new.claim(replacement)["reclaimed"])
        self.assertEqual(self.adapter.signals, [(100, signal.SIGTERM)])
        self.assert_error("superseded", old.release)
        self.assert_error("superseded", old.confirm)
        with new.registry as registry:
            self.assertEqual(registry.read()["session_id"], replacement["session_id"])
        self.adapter.listen(200, request["listen_port"])
        self.assertTrue(new.confirm()["ok"])
        self.assertTrue(new.release()["ok"])
        self.assertFalse(os.path.exists(new.registry.path))

    def test_equal_and_older_generations_cannot_kill_the_current_owner(self):
        old = self.manager()
        request = command(5)
        old.claim(request)
        for generation in (0, 4, 5):
            self.assert_error("superseded", self.manager(200).claim,
                              dict(request, generation=generation, session_id=str(uuid.uuid4())))
        self.assertEqual(self.adapter.signals, [])

    def test_unknown_listener_is_never_signaled_or_registered(self):
        self.adapter.listen(999, 12345)
        manager = self.manager()
        self.assert_error("unmanaged_conflict", manager.claim, command())
        self.assertEqual(self.adapter.signals, [])
        self.assertEqual(list(Path(self.directory.name).rglob("*.json")), [])

    def test_another_device_cannot_reclaim_the_first_devices_listener(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(100, 12345)
        self.assert_error("unmanaged_conflict", self.manager(200).claim,
                          dict(request, owner_id=str(uuid.uuid4()), generation=99))
        self.assertEqual(self.adapter.signals, [])

    def test_reused_pid_with_a_busy_port_cannot_be_killed(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(100, 12345)
        self.adapter.processes[100] = process(100, 1, birth="different birth")
        self.assert_error("unmanaged_conflict", self.manager(200).claim,
                          dict(request, generation=2, session_id=str(uuid.uuid4())))
        self.assertEqual(self.adapter.signals, [])

    def test_a_gone_session_and_free_port_allow_a_new_lease(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.processes.pop(100)
        self.adapter.tcp = [item for item in self.adapter.tcp if item["pid"] != 100]
        result = self.manager(200).claim(dict(request, generation=2, session_id=str(uuid.uuid4())))
        self.assertFalse(result["reclaimed"])
        self.assertEqual(self.adapter.signals, [])

    def test_wrong_helper_parent_or_transport_rejects_registered_pid(self):
        for tamper in ("parent", "transport", "helper_birth"):
            with self.subTest(tamper=tamper):
                # Independent owner/rule avoids records from previous cases.
                old = self.manager()
                request = command()
                old.claim(request)
                original = dict(self.adapter.processes[101])
                tcp = list(self.adapter.tcp)
                if tamper == "parent":
                    self.adapter.processes[101]["ppid"] = 200
                elif tamper == "transport":
                    self.adapter.tcp = [item for item in self.adapter.tcp if item["pid"] != 100]
                else:
                    self.adapter.processes[101]["birth"] = "reused helper"
                self.assert_error("ownership_mismatch", self.manager(200).claim,
                                  dict(request, generation=2, session_id=str(uuid.uuid4())))
                self.adapter.processes[101] = original
                self.adapter.tcp = tcp
        self.assertEqual(self.adapter.signals, [])

    def test_confirm_rejects_an_external_process_that_won_the_bind_race(self):
        manager = self.manager()
        manager.claim(command())
        self.adapter.listen(999, 12345)
        self.assert_error("unmanaged_conflict", manager.confirm)
        self.assertEqual(self.adapter.signals, [])

    def test_changing_to_an_occupied_port_keeps_the_old_managed_session_alive(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(100, 12345)
        self.adapter.listen(999, 12346)
        self.assert_error("unmanaged_conflict", self.manager(200).claim,
                          dict(request, generation=2, session_id=str(uuid.uuid4()), listen_port=12346))
        self.assertEqual(self.adapter.signals, [])
        self.assertIsNotNone(self.adapter.process(100))

    def test_directory_symlinks_and_world_readable_registry_are_rejected(self):
        request = command()
        owner = Path(self.directory.name) / "leases" / request["owner_id"]
        owner.parent.mkdir(mode=0o700)
        target = Path(self.directory.name) / "target"
        target.mkdir(mode=0o700)
        owner.symlink_to(target, target_is_directory=True)
        self.assert_error("permission_denied", helper.Registry, request["owner_id"], request["rule_id"])
        owner.unlink()
        owner.mkdir(mode=0o755)
        self.assert_error("permission_denied", helper.Registry, request["owner_id"], request["rule_id"])

    def test_invalid_ids_and_ports_are_rejected_before_creating_records(self):
        for overrides in ({"owner_id": "../../escape"}, {"rule_id": ""}, {"generation": True},
                          {"generation": -1}, {"listen_port": 0}, {"listen_port": 65536}):
            self.assert_error("invalid_request", self.manager().claim, command(**overrides))
        self.assertEqual(list(Path(self.directory.name).rglob("*.json")), [])


class OpaquePlatform(FakePlatform):
    """Normal Linux OpenSSH: root-created transport, user-created listener,
    with /proc/session/fd denied because the session is non-dumpable.
    """
    supports_opaque_fd = True

    def owns(self, pid, connection):
        return None

    def connected(self, pid, source_port):
        transport = super().connected(pid, source_port)
        self.tcp[-1].update(inode=str(100000 + source_port), uid=0)
        return transport

    def listen(self, pid, port):
        super().listen(pid, port)
        self.tcp[-1].update(inode=str(200000 + pid * 100 + port), uid=os.geteuid())


class OpaqueLinuxTests(unittest.TestCase):
    def setUp(self):
        RegistryTests.setUp(self)
        self.adapter = OpaquePlatform()
        self.old_transport = self.adapter.connected(100, 40001)
        self.new_transport = self.adapter.connected(200, 40002)

    manager = RegistryTests.manager
    assert_error = RegistryTests.assert_error

    def test_initial_confirm_requires_forward_success_ack_and_persists_kernel_identity(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(100, 12345)
        self.assert_error("ownership_mismatch", old.confirm)
        self.assert_error("ownership_mismatch", old.confirm, {"forward_ack": False})
        self.assertTrue(old.confirm({"forward_ack": True})["ok"])
        with old.registry as registry:
            record = registry.read()
        self.assertEqual(record["session_proof"]["source"], "ssh_exec_ancestry_inode")
        self.assertEqual(record["session_proof"]["socket"]["uid"], 0)
        self.assertEqual(record["listener_proof"]["source"], "ssh_forward_ack_inode")
        self.assertEqual(record["listener_proof"]["sockets"][0]["uid"], os.geteuid())
        self.assertEqual(record["listener_proof"]["sockets"][0]["inode"], self.adapter.tcp[-1]["inode"])
        new = self.manager(200)
        self.assertTrue(new.claim(dict(request, generation=2, session_id=str(uuid.uuid4())))["reclaimed"])
        self.assertEqual(self.adapter.signals, [(100, signal.SIGTERM)])

    def test_lost_confirm_reply_still_reclaims_only_the_registered_dedicated_session(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(100, 12345)
        # No confirm is ever sent. Ownership of the registered dedicated SSH
        # connection remains sufficient to close that connection itself.
        replacement = self.manager(200)
        result = replacement.claim(dict(request, generation=2, session_id=str(uuid.uuid4())))
        self.assertTrue(result["reclaimed"])
        self.assertEqual(self.adapter.signals, [(100, signal.SIGTERM)])

    def test_an_exited_helper_does_not_destroy_the_durable_session_identity_proof(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(100, 12345)
        old.confirm({"forward_ack": True})
        self.adapter.processes.pop(101)
        result = self.manager(200).claim(dict(request, generation=2, session_id=str(uuid.uuid4())))
        self.assertTrue(result["reclaimed"])
        self.assertEqual(self.adapter.signals, [(100, signal.SIGTERM)])

    def test_reused_transport_inode_or_replaced_listener_is_not_accepted(self):
        for tamper in ("transport", "listener"):
            with self.subTest(tamper=tamper):
                old = self.manager()
                request = command()
                old.claim(request)
                self.adapter.listen(100, 12345)
                old.confirm({"forward_ack": True})
                target = next(item for item in self.adapter.tcp if item["pid"] == 100 and
                              item["listening"] == (tamper == "listener"))
                original = target["inode"]
                target["inode"] = "99999999"
                self.assert_error("ownership_mismatch" if tamper == "transport" else "unmanaged_conflict",
                                  self.manager(200).claim, dict(request, generation=2, session_id=str(uuid.uuid4())))
                target["inode"] = original
                self.adapter.tcp = [item for item in self.adapter.tcp if not item["listening"]]
        self.assertEqual(self.adapter.signals, [])

    def test_claimed_session_recovery_never_signals_the_foreign_port_owner(self):
        old = self.manager()
        request = command()
        old.claim(request)
        self.adapter.listen(999, 12345)
        self.assert_error("unmanaged_conflict", self.manager(200).claim,
                          dict(request, generation=2, session_id=str(uuid.uuid4())))
        # The owned dedicated session may be closed; the foreign process is
        # never signaled and its listener remains an explicit conflict.
        self.assertEqual(self.adapter.signals, [(100, signal.SIGTERM)])
        self.assertEqual([item["pid"] for item in self.adapter.tcp if item["listening"]], [999])

    def test_forward_ack_does_not_override_an_incompatible_listener_uid(self):
        old = self.manager()
        old.claim(command())
        self.adapter.listen(100, 12345)
        self.adapter.tcp[-1]["uid"] = os.geteuid() + 1
        self.assert_error("unmanaged_conflict", old.confirm, {"forward_ack": True})
        self.assertEqual(self.adapter.signals, [])


FIXTURE_CODE = r'''
import json, os, signal, socket, subprocess, sys
control = socket.socket()
control.bind(("127.0.0.1", 0))
control.listen(1)
print(json.dumps({"accept_port": control.getsockname()[1]}), flush=True)
transport, _ = control.accept()
control.close()
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
if sys.argv[1] == "ignore-term":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
print(json.dumps({"helper_pid": child.pid, "transport": [transport.getpeername()[0], transport.getpeername()[1], transport.getsockname()[0], transport.getsockname()[1]]}), flush=True)
listener = None
try:
    for line in sys.stdin:
        request = json.loads(line)
        if request["op"] == "listen":
            listener = socket.socket()
            listener.bind(("127.0.0.1", request["port"]))
            listener.listen(1)
            print(json.dumps({"port": listener.getsockname()[1]}), flush=True)
        elif request["op"] == "quit":
            break
finally:
    child.terminate()
    child.wait()
'''


class ProcessFixture:
    def __init__(self, ignore_term=False):
        self.child = subprocess.Popen([sys.executable, "-u", "-c", FIXTURE_CODE,
                                       "ignore-term" if ignore_term else "normal"],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, text=True)
        self.witness = None
        self.peer = socket.create_connection(("127.0.0.1", self.read()["accept_port"]), timeout=5)
        ready = self.read()
        self.witness = ready["helper_pid"]
        self.transport = ready["transport"]

    def read(self):
        line = self.child.stdout.readline()
        if not line:
            raise AssertionError("fixture process stopped unexpectedly: " + self.child.stderr.read())
        return json.loads(line)

    def listen(self, port=0):
        self.child.stdin.write(json.dumps(dict(op="listen", port=port)) + "\n")
        self.child.stdin.flush()
        return self.read()["port"]

    def close(self):
        self.peer.close()
        if self.child.poll() is None:
            with contextlib.suppress(BrokenPipeError):
                self.child.stdin.write('{"op":"quit"}\n')
                self.child.stdin.flush()
            try:
                self.child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait()
        # A SIGKILL'ed fixture cannot reap its witness. This PID was returned by
        # our isolated fixture, never discovered from the user's SSH processes.
        if self.witness is not None:
            with contextlib.suppress(ProcessLookupError):
                os.kill(self.witness, signal.SIGTERM)
        self.child.stdin.close()
        self.child.stdout.close()
        self.child.stderr.close()


class FixturePlatform:
    """Use real OS identities/sockets but recognize only our fake SSH fixtures."""
    def __init__(self, fixtures):
        self.native = helper.platform_adapter()
        self.fixture_pids = {item.child.pid for item in fixtures}

    def process(self, pid):
        record = self.native.process(pid)
        if record is not None and pid in self.fixture_pids:
            record["name"] = "sshd"
        return record

    def sockets(self):
        return self.native.sockets()

    def owns(self, pid, connection):
        return self.native.owns(pid, connection)

    def open_process(self, identity):
        self.assert_fixture(identity)
        return self.native.open_process(self.native.process(identity["pid"]))

    def signal_process(self, descriptor, identity, sig):
        self.assert_fixture(identity)
        current = self.native.process(identity["pid"])
        if current is not None and current["birth"] == identity["birth"]:
            self.native.signal_process(descriptor, current, sig)

    def close_process(self, descriptor):
        self.native.close_process(descriptor)

    def assert_fixture(self, identity):
        if identity["pid"] not in self.fixture_pids:
            raise AssertionError("test attempted to signal a non-fixture process")


@unittest.skipUnless(sys.platform.startswith("linux") or sys.platform == "darwin", "Unix remote helper")
class RealProcessTests(unittest.TestCase):
    def test_only_registered_fixture_is_terminated_and_port_is_reused(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        environment = mock.patch.dict(os.environ, FWM_REMOTE_STATE_DIR=directory.name)
        environment.start()
        self.addCleanup(environment.stop)
        old, new, unrelated = ProcessFixture(ignore_term=True), ProcessFixture(), ProcessFixture()
        for item in (old, new, unrelated):
            self.addCleanup(item.close)
        adapter = FixturePlatform((old, new, unrelated))
        target_port = old.listen()
        request = command(listen_port=target_port)

        def manager(fixture):
            instance = helper.LeaseHelper(adapter)
            instance.identify = lambda: (adapter.process(fixture.child.pid), adapter.process(fixture.witness), fixture.transport)
            return instance

        # Register the fixture as if its initial claim preceded its real bind.
        previous = manager(old)
        session, witness, transport = previous.identify()
        record = dict(helper.validate_claim(request), protocol=1, phase="confirmed",
                      session=session, helper=witness, transport=transport)
        with helper.Registry(request["owner_id"], request["rule_id"]) as registry:
            registry.write(record)
        self.assertTrue(helper.connection_owned(adapter, session, transport))
        self.assertTrue(helper.listeners(adapter, "127.0.0.1", target_port))
        replacement = manager(new)
        with mock.patch.object(helper, "TERM_SECONDS", 0.1):
            result = replacement.claim(dict(request, generation=2, session_id=str(uuid.uuid4())))
        self.assertTrue(result["reclaimed"])
        old.child.wait(timeout=3)
        self.assertEqual(old.child.returncode, -signal.SIGKILL)
        self.assertIsNone(unrelated.child.poll(), "unrelated process must remain alive")
        self.assertTrue(helper.port_available("127.0.0.1", target_port))
        self.assertEqual(new.listen(target_port), target_port)
        self.assertTrue(replacement.confirm()["ok"])
        self.assertTrue(replacement.release()["ok"])

    def test_unregistered_real_listener_is_not_signaled(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        environment = mock.patch.dict(os.environ, FWM_REMOTE_STATE_DIR=directory.name)
        environment.start()
        self.addCleanup(environment.stop)
        fixture = ProcessFixture()
        self.addCleanup(fixture.close)
        adapter = FixturePlatform((fixture,))
        port = fixture.listen()
        manager = helper.LeaseHelper(adapter)
        manager.identify = lambda: (adapter.process(fixture.child.pid), adapter.process(fixture.witness), fixture.transport)
        with self.assertRaises(helper.LeaseError) as caught:
            manager.claim(command(listen_port=port))
        self.assertEqual(caught.exception.code, "unmanaged_conflict")
        self.assertIsNone(fixture.child.poll())
        self.assertFalse(helper.port_available("127.0.0.1", port))


class ParsingTests(unittest.TestCase):
    def test_json_lines_protocol_does_not_emit_tracebacks_or_noise(self):
        result = subprocess.run([sys.executable, "-u", str(SCRIPT)],
                                input='not-json\n{"op":"unknown"}\n{"op":"confirm"}\n',
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        replies = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual(len(replies), 3)
        self.assertEqual([reply["code"] for reply in replies], ["invalid_request"] * 3)
        self.assertEqual(result.stderr, "")
        self.assertTrue(all(reply["protocol"] == 1 and not reply["ok"] for reply in replies))

    def test_a_local_shell_cannot_claim_an_ssh_session_from_a_spoofed_environment(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        result = subprocess.run([sys.executable, "-u", str(SCRIPT)],
                                input=json.dumps(command()) + "\n", text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True,
                                env=dict(os.environ, SSH_CONNECTION="127.0.0.1 40001 127.0.0.1 22",
                                         FWM_REMOTE_STATE_DIR=directory.name))
        reply = json.loads(result.stdout)
        self.assertFalse(reply["ok"])
        self.assertEqual(reply["code"], "ownership_mismatch")
        self.assertEqual(list(Path(directory.name).iterdir()), [])
        self.assertEqual(result.stderr, "")

    def test_lsof_ipv4_ipv6_established_and_listener_records(self):
        records = helper.MacPlatform.parse_lsof(
            "p123\nf5\nn127.0.0.1:22->127.0.0.1:40001\nTST=ESTABLISHED\n"
            "f6\nn[::1]:12222\nTST=LISTEN\np456\nf7\ntIPv4\nn*:9999\nTST=LISTEN\n")
        self.assertEqual(records[0]["remote"], ("127.0.0.1", 40001))
        self.assertFalse(records[0]["listening"])
        self.assertEqual(records[1]["local"], ("::1", 12222))
        self.assertTrue(records[1]["listening"])
        self.assertEqual(records[2]["pid"], 456)
        self.assertEqual(records[2]["local"], ("0.0.0.0", 9999))

    def test_lsof_wildcards_preserve_address_family(self):
        records = helper.MacPlatform.parse_lsof(
            "p123\nf5\ntIPv6\nn*:12222\nTST=LISTEN\n"
            "f6\ntIPv4\nn*:12223\nTST=LISTEN\n")
        self.assertEqual(records[0]["local"], ("::", 12222))
        self.assertEqual(records[1]["local"], ("0.0.0.0", 12223))
        with self.assertRaises(helper.LeaseError):
            helper.MacPlatform.parse_lsof("p123\nf5\nn*:12222\nTST=LISTEN\n")

    def test_linux_procfs_byte_order_matches_loopback_addresses(self):
        self.assertEqual(helper.LinuxPlatform.address("0100007F:0016"), ("127.0.0.1", 22))
        self.assertEqual(helper.LinuxPlatform.address("00000000000000000000000001000000:0016"), ("::1", 22))


if __name__ == "__main__":
    unittest.main()
