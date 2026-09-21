#!/usr/bin/env python3
"""Read-only product audit using isolated configs and refused loopback SSH only.

Writes only this report's result JSON and temporary fixtures. Does not modify
production code, the default daemon, real services or SSH credentials.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import stat
import subprocess
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[2]
RECORDS = []


class Fixture:
    def __init__(self, binary, directory):
        self.binary = binary
        self.directory = directory
        directory.mkdir(parents=True)
        (directory / 'ssh.conf').write_text('Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n')
        self.calls = []

    def cli(self, *args, expected=0):
        process = subprocess.run([str(self.binary), '--config-dir', str(self.directory), '--json', *args],
                                 capture_output=True, text=True, timeout=20)
        try:
            result = json.loads(process.stdout or process.stderr)
        except ValueError:
            result = {'stdout': process.stdout, 'stderr': process.stderr}
        self.calls.append({'args': list(args), 'exit': process.returncode, 'result': result})
        if expected is not None:
            assert process.returncode == expected, self.calls[-1]
        return result

    def init(self, *identity):
        return self.cli('server', 'add', 'dev', '--host', '127.0.0.1', '--port', '1',
                        '--ssh-config', str(self.directory / 'ssh.conf'), *identity)

    def add(self, name, port):
        return self.cli('add', name, '--server', 'dev', '--local', '--port', str(port), '--disabled')

    def config(self):
        return self.cli('config', 'export')

    def stop(self):
        subprocess.run([str(self.binary), '--config-dir', str(self.directory), 'daemon', 'stop'],
                       capture_output=True, text=True, timeout=20)

    def rpc(self, command, revision):
        request = {'api_version': 1, 'request_id': str(uuid.uuid4()), 'expected_revision': revision, 'command': command}
        encoded = json.dumps(request).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(10)
            stream.connect(str(self.directory / 'state/daemon.sock'))
            stream.sendall(struct.pack('>I', len(encoded)) + encoded)
            def exact(length):
                result = bytearray()
                while len(result) < length:
                    data = stream.recv(length - len(result))
                    assert data, 'premature IPC EOF'
                    result.extend(data)
                return bytes(result)
            result = json.loads(exact(struct.unpack('>I', exact(4))[0]))
        self.calls.append({'rpc': request, 'response': result})
        return result


def controls_follow_wrong_name(binary, root):
    for action in ('remove', 'down', 'up'):
        f = Fixture(binary, root / ('control-' + action))
        try:
            f.init()
            f.add('one', 31101)
            f.add('two', 31102)
            if action == 'down':
                text = (f.directory / 'config.toml').read_text()
                tail = text.rfind('desired_state = "stopped"')
                text = text[:tail] + text[tail:].replace('desired_state = "stopped"', 'desired_state = "running"', 1)
                (f.directory / 'config.toml').write_text(text)
                f.cli('config', 'reload')
            before = f.config()
            expected_id = before['forwards'][1]['id']
            draft = (f.directory / 'config.toml').read_text().replace('name = "one"', 'name = "three"').replace('name = "two"', 'name = "one"')
            (f.directory / 'config.toml').write_text(draft)
            f.cli('config', 'validate')
            f.cli(action, 'one')
            if action == 'up':
                f.cli('daemon', 'stop')
            f.cli('config', 'reload')
            after = f.config()
            survivor = next((rule for rule in after['forwards'] if rule['id'] == expected_id), None)
            expected = before['forwards'][1]['desired_state']
            actual = None if survivor is None else survivor['desired_state']
            assert actual != expected, (action, expected, actual)
            RECORDS.append({'case': 'control-name-fallback-' + action, 'expected_untouched_id': expected_id,
                            'expected_state': expected, 'actual_state': actual, 'calls': f.calls})
        finally:
            f.stop()


def recovery_revision_reuse(binary, root):
    f = Fixture(binary, root / 'revision')
    try:
        created = f.init()['data']['config']
        stale_profile = copy.deepcopy(created['servers'][0])
        stale_profile['user'] = 'stale-client-change'
        for port in ('2', '3', '4'):
            f.cli('server', 'edit', 'dev', '--port', port)
        before = f.config()
        path = f.directory / 'config.toml'
        path.write_text(re.sub(r'^revision = \d+', 'revision = 0', path.read_text(), flags=re.M))
        recovered = f.cli('config', 'recover', '--from-candidate')['data']['config']
        assert recovered['revision'] < before['revision']
        f.cli('daemon', 'start')
        stale = f.rpc({'method': 'put_server', 'params': {'server': stale_profile}}, created['revision'])
        assert stale['ok'] and stale['data']['config']['servers'][0]['port'] == 1
        RECORDS.append({'case': 'recovery-reuses-stale-cas-revision', 'before_revision': before['revision'],
                        'recovered_revision': recovered['revision'], 'stale_request_accepted': stale['ok'],
                        'before_port': before['servers'][0]['port'], 'after_stale_port': stale['data']['config']['servers'][0]['port'],
                        'calls': f.calls})
    finally:
        f.stop()


def path_failure_blocks_configuration(binary, root):
    f = Fixture(binary, root / 'path')
    keys = root / 'keys'
    (keys / 'sub').mkdir(parents=True)
    try:
        f.init('--identity', str(keys / 'sub/id'))
        f.add('web', 31119)
        f.cli('up', 'web')
        shutil.rmtree(keys)
        keys.write_text('a parent directory became a regular file')
        f.cli('down', 'web', expected=5)
        f.cli('server', 'edit', 'dev', '--unset', 'identity', expected=5)
        status = f.cli('status', 'web')
        assert status['forwards'][0]['desired_state'] == 'running'
        f.cli('daemon', 'stop')
        f.cli('config', 'export', expected=2)
        f.cli('remove', 'web', expected=2)
        # Control: restoring only the path layout, without editing TOML, makes
        # the same snapshot readable and the same requested stop succeed.
        keys.unlink()
        (keys / 'sub').mkdir(parents=True)
        f.cli('down', 'web')
        RECORDS.append({'case': 'ssh-path-enotdir-blocks-control-plane', 'calls': f.calls})
    finally:
        f.stop()


def inferred_legacy_group_invalidates_config(binary, root):
    f = Fixture(binary, root / 'legacy')
    try:
        source = '''schema_version = 2
revision = 0
[[servers]]
id = "server"
name = "dev"
host = "127.0.0.1"
port = 1
[[forwards]]
id = "first"
name = "bundle-3000"
server_id = "server"
kind = "local"
listen = "127.0.0.1:3000"
target = "localhost:80"
desired_state = "stopped"
[[forwards]]
id = "second"
name = "bundle-03000"
server_id = "server"
kind = "local"
listen = "127.0.0.1:3000"
target = "localhost:81"
desired_state = "stopped"
'''
        path = f.directory / 'config.toml'
        path.write_text(source)
        failure = f.cli('config', 'validate', expected=2)
        assert 'conflict within group' in failure['error']['message']
        path.write_text(source.replace('schema_version = 2', 'schema_version = 3'))
        f.cli('config', 'validate')
        RECORDS.append({'case': 'legacy-group-inference-creates-new-conflict', 'calls': f.calls})
    finally:
        f.stop()


def recovery_mirror_failure_loses_protection(binary, root):
    if not hasattr(os, 'chflags') or not hasattr(stat, 'UF_IMMUTABLE'):
        RECORDS.append({'case': 'recovery-mirror-failure', 'skipped': 'requires reversible macOS user immutable file flag'})
        return
    f = Fixture(binary, root / 'recovery-mirror')
    path = f.directory / 'config.toml'
    try:
        f.init()
        f.add('deleted', 31122)
        f.add('survivor', 31123)
        path.write_text(path.read_text().replace('desired_state = "stopped"', 'desired_state = "running"'))
        f.cli('remove', 'deleted')
        snapshot = f.directory / 'state/applied.toml'
        header = snapshot.read_text().splitlines()[0]
        assert header.startswith('# fwm-control-overrides:')
        snapshot.write_text(header + '\nmalformed = [\n')
        os.chflags(path, stat.UF_IMMUTABLE)
        recovered = f.cli('config', 'recover', '--from-candidate')
        assert recovered['data']['warning'] and 'could not be updated' in recovered['data']['warning']
        assert [rule['name'] for rule in recovered['data']['config']['forwards']] == ['survivor']
        assert recovered['data']['config']['forwards'][0]['desired_state'] == 'stopped'
        assert not snapshot.read_text().startswith('# fwm-control-overrides:')
        os.chflags(path, 0)
        reloaded = f.cli('config', 'reload')['data']['config']
        assert {rule['name'] for rule in reloaded['forwards']} == {'deleted', 'survivor'}
        assert all(rule['desired_state'] == 'running' for rule in reloaded['forwards'])
        RECORDS.append({'case': 'recovery-mirror-failure-loses-stop-and-delete-protection', 'calls': f.calls})
    finally:
        if path.exists():
            os.chflags(path, 0)
        f.stop()


def missing_snapshot_bootstraps_unapplied_draft(binary, root):
    f = Fixture(binary, root / 'lost-snapshot')
    try:
        f.init()
        f.add('web', 31124)
        f.cli('daemon', 'start')
        f.cli('daemon', 'stop')
        assert (f.directory / 'state/recovery.json').exists()
        candidate = f.directory / 'config.toml'
        candidate.write_text(candidate.read_text().replace('desired_state = "stopped"', 'desired_state = "running"'))
        (f.directory / 'state/applied.toml').unlink()
        f.cli('daemon', 'start')
        status = f.cli('status', 'web')
        assert status['forwards'][0]['desired_state'] == 'running'
        assert not status['warnings']
        RECORDS.append({'case': 'missing-snapshot-silently-applies-draft-at-start', 'calls': f.calls})
    finally:
        f.stop()


def dangling_candidate_link_is_overwritten(binary, root):
    if os.name != 'posix':
        return
    f = Fixture(binary, root / 'dangling-candidate')
    path = f.directory / 'config.toml'
    try:
        path.symlink_to(f.directory / 'missing-target')
        assert f.config()['servers'] == []
        assert path.is_symlink()
        f.init()
        assert not path.is_symlink() and path.is_file()
        RECORDS.append({'case': 'dangling-config-link-treated-as-absent-and-replaced', 'calls': f.calls})
    finally:
        f.stop()


def final_controls_and_file_matrix(binary, root):
    f = Fixture(binary, root / 'negative-controls')
    try:
        f.init('--identity', str(root / 'missing/parents/id'))
        f.add('reused', 31125)
        old_id = f.config()['forwards'][0]['id']
        f.cli('remove', 'reused')
        f.add('reused', 31125)
        new_id = f.config()['forwards'][0]['id']
        assert new_id != old_id
        f.cli('config', 'reload')
        assert f.config()['forwards'][0]['id'] == new_id
        f.cli('down', 'reused')
        candidate = f.directory / 'config.toml'
        valid = candidate.read_text()
        candidate.write_text('schema_version = [unfinished draft')
        f.cli('down', 'reused')
        assert candidate.read_text() == 'schema_version = [unfinished draft'
        candidate.write_text(valid.replace('desired_state = "stopped"', 'desired_state = "running"'))
        f.cli('config', 'reload')
        assert f.config()['forwards'][0]['desired_state'] == 'stopped'
        assert not (f.directory / 'state/applied.toml').read_text().startswith('# fwm-control-overrides:')
        # Once the old draft has been reconciled and protection retired,
        # a later intentional hand edit can change saved intent.
        candidate.write_text(candidate.read_text().replace('desired_state = "stopped"', 'desired_state = "running"'))
        f.cli('config', 'reload')
        assert f.config()['forwards'][0]['desired_state'] == 'running'
        f.cli('down', 'reused')
        f.cli('remove', 'reused')
        RECORDS.append({'case': 'negative-controls-final-pass', 'expected_behavior_confirmed': True,
                        'covers': ['completed-delete-name-reuse', 'missing-identity-parents', 'invalid-draft-stop',
                                   'preserved-stop-through-reload', 'retired-overlay-allows-later-explicit-edit'], 'calls': f.calls})
    finally:
        f.stop()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/fwm')
    parser.add_argument('--output', type=Path, default=Path(__file__).with_name('storage-results.json'))
    args = parser.parse_args()
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix='fwm-postfix-storage-', dir='/private/tmp') as temporary:
        root = Path(temporary)
        controls_follow_wrong_name(binary, root)
        recovery_revision_reuse(binary, root)
        path_failure_blocks_configuration(binary, root)
        inferred_legacy_group_invalidates_config(binary, root)
        recovery_mirror_failure_loses_protection(binary, root)
        missing_snapshot_bootstraps_unapplied_draft(binary, root)
        dangling_candidate_link_is_overwritten(binary, root)
        final_controls_and_file_matrix(binary, root)
    args.output.write_text(json.dumps({'binary': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                                       'cases': RECORDS}, indent=2, ensure_ascii=False))
    print(json.dumps({'confirmed_cases': [case['case'] for case in RECORDS], 'output': str(args.output)}, ensure_ascii=False))


if __name__ == '__main__':
    main()
