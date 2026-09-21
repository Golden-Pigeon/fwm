#!/usr/bin/env python3
"""Bounded FIFO SSH-config probe against a self-owned foreground daemon.

No SSH connection or forward is created. The FIFO is always fed and closed in
finally before this script stops its own process, including on assertion errors.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
PAYLOAD = b'Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n'


def send(path, command, revision=None):
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(2)
    stream.connect(str(path))
    request = {'api_version': 1, 'request_id': str(uuid.uuid4()), 'expected_revision': revision, 'command': command}
    data = json.dumps(request).encode()
    stream.sendall(struct.pack('>I', len(data)) + data)
    return stream, request


def receive(stream, timeout):
    stream.settimeout(timeout)
    def exact(count):
        chunks = bytearray()
        while len(chunks) < count:
            data = stream.recv(count - len(chunks))
            if not data:
                raise EOFError('private IPC closed before the response')
            chunks.extend(data)
        return bytes(chunks)
    return json.loads(exact(struct.unpack('>I', exact(4))[0]))


def rpc(path, command, timeout=2):
    stream, _ = send(path, command)
    try:
        return receive(stream, timeout)
    finally:
        stream.close()


def feed_fifo(path):
    try:
        fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
    except OSError as error:
        if error.errno in (errno.ENXIO, errno.ENOENT):
            return False
        raise
    try:
        os.write(fd, PAYLOAD)
    finally:
        os.close(fd)
    return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/fwm')
    parser.add_argument('--output', type=Path, default=Path(__file__).with_suffix('.json'))
    args = parser.parse_args()
    binary = args.binary.resolve()
    result = {'binary': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'configured_connect_timeout_secs': 1, 'forward_count': 0}
    with tempfile.TemporaryDirectory(prefix='fwm-fifo-audit-', dir='/private/tmp') as directory:
        root = Path(directory)
        profile = root / 'manager'
        profile.mkdir()
        (profile / 'config.toml').write_text('schema_version = 3\n[defaults.retry]\nconnect_timeout_secs = 1\n')
        fifo = root / 'ssh_config.fifo'
        os.mkfifo(fifo, 0o600)
        ipc = profile / 'state/daemon.sock'
        log = (root / 'foreground.log').open('wb')
        daemon = subprocess.Popen([str(binary), '--config-dir', str(profile), 'daemon', 'run'],
                                  stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        result['foreground_pid'] = daemon.pid
        streams = []
        try:
            deadline = time.monotonic() + 5
            while True:
                assert daemon.poll() is None, (root / 'foreground.log').read_text()
                try:
                    result['ping_before'] = rpc(ipc, {'method': 'ping'})
                    break
                except (OSError, EOFError):
                    assert time.monotonic() < deadline, 'temporary daemon did not become ready'
                    time.sleep(.02)
            server = {'id': str(uuid.uuid4()), 'name': 'fifo-only', 'host': '127.0.0.1',
                      'port': 1, 'user': 'fixture', 'ssh_config': str(fifo)}
            mutation, request = send(ipc, {'method': 'put_server', 'params': {'server': server}}, 0)
            streams.append(mutation)
            result['request'] = request
            started = time.monotonic()
            try:
                result['unexpected_early_mutation'] = receive(mutation, 1)
                result['mutation_timed_out'] = False
            except TimeoutError:
                result['mutation_timed_out'] = True
            ping, _ = send(ipc, {'method': 'ping'})
            streams.append(ping)
            try:
                result['unexpected_early_ping'] = receive(ping, 1)
                result['ping_timed_out'] = False
            except TimeoutError:
                result['ping_timed_out'] = True
            result['seconds_before_fifo_release'] = time.monotonic() - started
            assert result['mutation_timed_out'] and result['ping_timed_out'], result
            result['fifo_released'] = feed_fifo(fifo)
            assert result['fifo_released'], 'blocked reader was not waiting on the fixture FIFO'
            result['mutation_after_release'] = receive(mutation, 3)
            result['pending_ping_after_release'] = receive(ping, 3)
            result['fresh_ping_after_release'] = rpc(ipc, {'method': 'ping'})
            assert result['mutation_after_release']['ok']
            assert result['pending_ping_after_release']['ok'] and result['fresh_ping_after_release']['ok']
            assert result['mutation_after_release']['data']['config']['forwards'] == []
        finally:
            # A normal writer would block if no reader existed; nonblocking
            # attempts keep cleanup bounded while releasing any late reader.
            for _ in range(30):
                if feed_fifo(fifo) or daemon.poll() is not None:
                    break
                time.sleep(.02)
            for stream in streams:
                stream.close()
            if daemon.poll() is None:
                try:
                    rpc(ipc, {'method': 'shutdown'}, timeout=2)
                    daemon.wait(timeout=3)
                except (OSError, EOFError, subprocess.TimeoutExpired):
                    feed_fifo(fifo)
                    daemon.terminate()
                    try:
                        daemon.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        daemon.kill()
                        daemon.wait(timeout=3)
            result['foreground_returncode'] = daemon.returncode
            result['foreground_cleaned'] = daemon.poll() is not None
            log.close()
            args.output.write_text(json.dumps(result, indent=2))
    print(json.dumps({'confirmed': result['mutation_timed_out'] and result['ping_timed_out'],
                      'recovered_after_fifo_write': result['fresh_ping_after_release']['ok'],
                      'cleaned': result['foreground_cleaned'], 'output': str(args.output)}))


if __name__ == '__main__':
    main()
