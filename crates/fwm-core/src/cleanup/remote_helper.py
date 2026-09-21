"""FWM verified SSH-session leases. Embedded and executed with Python 3 -u.

No shell subprocesses or remote installation are used. The only processes this
helper may signal are a previously registered, independently re-verified SSH
session for the same owner and rule. stdout is exclusively JSON-lines protocol.
"""

import ctypes
import errno
import ipaddress
import json
import os
import re
import signal
import socket
import stat
import subprocess
import sys
import time
import uuid

try:
    import fcntl
except ImportError:
    # A mistakenly selected Windows SSH server still gets a protocol error,
    # rather than a Python traceback that the client cannot classify.
    fcntl = None


PROTOCOL = 1
MAX_LINE = 65536
TERM_SECONDS = 2.0
KILL_SECONDS = 2.0


class LeaseError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def failure(code, message):
    raise LeaseError(code, message)


def ip(value):
    try:
        parsed = ipaddress.ip_address(value.split("%", 1)[0])
        if isinstance(parsed, ipaddress.IPv6Address) and parsed.ipv4_mapped:
            return parsed.ipv4_mapped
        return parsed
    except (ValueError, AttributeError):
        failure("unsupported", "verified cleanup requires a literal IP listen/SSH address")


def endpoint(value):
    if value.startswith("["):
        host, port = value[1:].rsplit("]:", 1)
    else:
        host, port = value.rsplit(":", 1)
    return (str(ip(host)), int(port))


def ssh_connection(value):
    fields = value.split()
    if len(fields) != 4:
        failure("ownership_mismatch", "SSH_CONNECTION is missing or malformed")
    try:
        client_port, server_port = int(fields[1]), int(fields[3])
    except ValueError:
        failure("ownership_mismatch", "SSH_CONNECTION contains invalid ports")
    if not (0 < client_port <= 65535 and 0 < server_port <= 65535):
        failure("ownership_mismatch", "SSH_CONNECTION contains invalid ports")
    return [str(ip(fields[0])), client_port, str(ip(fields[2])), server_port]


def overlaps(a, b):
    left, right = ip(a), ip(b)
    # An IPv6 wildcard may also own the IPv4 bind, depending on IPV6_V6ONLY.
    # Conservatively treat both wildcard families as overlapping.
    if left.version != right.version:
        return (left.version == 6 and left.is_unspecified or
                right.version == 6 and right.is_unspecified)
    return left == right or left.is_unspecified or right.is_unspecified


def same_process(left, right):
    return left is not None and right is not None and all(
        left.get(key) == right.get(key) for key in ("pid", "birth", "uid", "name")
    )


def run_readonly(args):
    try:
        result = subprocess.run(
            args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, timeout=5, check=False,
            env=dict(os.environ, LC_ALL="C"), text=True,
        )
    except FileNotFoundError:
        failure("unsupported", "required remote utility is unavailable: " + args[0])
    except subprocess.TimeoutExpired:
        failure("unsupported", "remote process inspection timed out: " + args[0])
    if result.returncode not in (0, 1):
        failure("permission_denied", "cannot inspect remote TCP ownership: " + result.stderr.strip())
    return result.stdout


class LinuxPlatform:
    """Birth identity from procfs, network identity from socket inode ownership."""

    supports_opaque_fd = True

    def process(self, pid):
        try:
            with open("/proc/{}/stat".format(pid), encoding="utf8") as stream:
                value = stream.read()
            close = value.rfind(")")
            name = value[value.index("(") + 1:close]
            fields = value[close + 2:].split()
            with open("/proc/{}/status".format(pid), encoding="utf8") as stream:
                status = stream.read()
            uid = int(re.search(r"^Uid:\s+(\d+)", status, re.M).group(1))
            with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as stream:
                boot = stream.read().strip()
            return {"pid": pid, "ppid": int(fields[1]), "birth": boot + ":" + fields[19],
                    "uid": uid, "name": name, "state": fields[0]}
        except (FileNotFoundError, ProcessLookupError):
            return None
        except PermissionError:
            failure("permission_denied", "cannot inspect process {} in procfs".format(pid))
        except (ValueError, IndexError, AttributeError):
            failure("unsupported", "unrecognized Linux procfs process format")

    @staticmethod
    def address(value):
        host, port = value.split(":")
        raw = b"".join(int(host[index:index + 8], 16).to_bytes(4, sys.byteorder)
                       for index in range(0, len(host), 8))
        return str(ip(str(ipaddress.ip_address(raw)))), int(port, 16)

    def sockets(self):
        result = []
        for path in ("/proc/net/tcp", "/proc/net/tcp6"):
            try:
                with open(path, encoding="ascii") as stream:
                    for line in list(stream)[1:]:
                        columns = line.split()
                        local, remote = self.address(columns[1]), self.address(columns[2])
                        result.append({"local": local, "remote": remote,
                                       "listening": columns[3] == "0A", "inode": columns[9],
                                       "uid": int(columns[7])})
            except FileNotFoundError:
                continue
            except PermissionError:
                failure("permission_denied", "cannot inspect Linux TCP sockets")
        return result

    def owns(self, pid, connection):
        directory = "/proc/{}/fd".format(pid)
        try:
            with os.scandir(directory) as entries:
                for entry in entries:
                    try:
                        if os.readlink(entry.path) == "socket:[{}]".format(connection["inode"]):
                            return True
                    except FileNotFoundError:
                        continue
            return False
        except FileNotFoundError:
            return False
        except PermissionError:
            # OpenSSH intentionally marks its setuid session non-dumpable.
            # Same-UID fd inspection is then denied on normal Linux installs.
            # Callers distinguish this from a readable-but-unowned descriptor;
            # only the former may use our recorded SSH-exec/inode proof.
            return None

    def open_process(self, identity):
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            failure("unsupported", "safe Linux session termination requires Python 3.9+ and kernel pidfd support")
        try:
            descriptor = os.pidfd_open(identity["pid"], 0)
        except ProcessLookupError:
            return None
        except OSError as error:
            failure("unsupported" if error.errno in (errno.ENOSYS, errno.EINVAL) else "permission_denied",
                    "cannot pin old SSH process identity: " + str(error))
        if not same_process(identity, self.process(identity["pid"])):
            os.close(descriptor)
            failure("ownership_mismatch", "old SSH process identity changed before pidfd pinning")
        return descriptor

    def signal_process(self, descriptor, identity, sig):
        try:
            signal.pidfd_send_signal(descriptor, sig, None, 0)
        except ProcessLookupError:
            pass
        except PermissionError:
            failure("permission_denied", "the remote user cannot terminate its old SSH session")

    @staticmethod
    def close_process(descriptor):
        if descriptor is not None:
            os.close(descriptor)


class ProcBsdInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint32) for name in (
        "flags", "status", "xstatus", "pid", "ppid", "uid", "gid", "ruid", "rgid", "svuid", "svgid", "rfu"
    )] + [("comm", ctypes.c_char * 16), ("name", ctypes.c_char * 32)] + [
        (name, ctypes.c_uint32) for name in ("nfiles", "pgid", "pjobc", "tdev", "tpgid")
    ] + [("nice", ctypes.c_int32), ("start_sec", ctypes.c_uint64), ("start_usec", ctypes.c_uint64)]


class MacPlatform:
    """libproc birth timestamps plus lsof TCP identities, without privileged tools."""

    def __init__(self):
        self.lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_pidinfo.restype = ctypes.c_int

    def process(self, pid):
        info = ProcBsdInfo()
        ctypes.set_errno(0)
        size = self.lib.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
        if size == 0 and ctypes.get_errno() in (0, errno.ESRCH):
            return None
        if size != ctypes.sizeof(info):
            failure("permission_denied", "cannot inspect process {} with libproc".format(pid))
        name = bytes(info.comm).split(b"\0", 1)[0].decode("utf8", "replace")
        return {"pid": int(info.pid), "ppid": int(info.ppid),
                "birth": "{}:{}".format(info.start_sec, info.start_usec),
                "uid": int(info.uid), "name": name, "state": str(info.status)}

    @staticmethod
    def parse_lsof(text):
        result, current, pid = [], None, None
        for line in text.splitlines():
            if not line:
                continue
            field, value = line[0], line[1:]
            if field in ("p", "f"):
                if current and "name" in current:
                    result.append(current)
                current = None
                if field == "p":
                    pid = int(value)
                else:
                    current = {"pid": pid, "fd": value, "listening": False}
            elif current is not None and field == "t":
                current["family"] = value
            elif current is not None and field == "n":
                current["name"] = value
            elif current is not None and field == "T" and value == "ST=LISTEN":
                current["listening"] = True
        if current and "name" in current:
            result.append(current)
        sockets = []
        for item in result:
            try:
                names = item.pop("name").split("->")
                # The name alone is ambiguous; lsof's t field carries the family.
                if names[0].startswith("*:"):
                    family = item.get("family")
                    if family not in ("IPv4", "IPv6"):
                        failure("unsupported", "lsof wildcard socket has no address family")
                    names[0] = ("[::]:" if family == "IPv6" else "0.0.0.0:") + names[0][2:]
                item.pop("family", None)
                local = endpoint(names[0])
                remote = endpoint(names[1]) if len(names) == 2 else ("0.0.0.0", 0)
                item.update(local=local, remote=remote)
                sockets.append(item)
            except (ValueError, LeaseError):
                failure("unsupported", "cannot parse lsof TCP socket identity")
        return sockets

    def sockets(self):
        return self.parse_lsof(run_readonly(["/usr/sbin/lsof", "-nP", "-iTCP", "-F", "pftnT"]))

    @staticmethod
    def owns(pid, connection):
        return connection["pid"] == pid

    def open_process(self, identity):
        # macOS has no pidfd equivalent. Keep the descendant helper identity in
        # every check as an additional lifetime witness before each signal.
        if not same_process(identity, self.process(identity["pid"])):
            failure("ownership_mismatch", "old SSH process identity changed")
        return identity["pid"]

    def signal_process(self, descriptor, identity, sig):
        if not same_process(identity, self.process(descriptor)):
            failure("ownership_mismatch", "refusing to signal a reused SSH process id")
        try:
            os.kill(descriptor, sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            failure("permission_denied", "the remote user cannot terminate its old SSH session")

    @staticmethod
    def close_process(descriptor):
        pass


def platform_adapter():
    if sys.platform.startswith("linux"):
        return LinuxPlatform()
    if sys.platform == "darwin":
        return MacPlatform()
    failure("unsupported", "verified cleanup supports Linux and macOS SSH servers")


def ancestors(adapter, pid, limit=32):
    seen = set()
    for _ in range(limit):
        if pid <= 1 or pid in seen:
            return
        seen.add(pid)
        process = adapter.process(pid)
        if process is None:
            return
        yield process
        pid = process["ppid"]


def socket_identity(connection):
    result = {"local": list(connection["local"]), "remote": list(connection["remote"]),
              "listening": connection["listening"]}
    for name in ("inode", "uid", "pid", "fd"):
        if name in connection:
            result[name] = connection[name]
    return result


def transport_proof(adapter, process, transport, witness=None, registered=None):
    client_host, client_port, server_host, server_port = transport
    matching = [connection for connection in adapter.sockets()
                if not connection["listening"] and
                connection["local"] == (server_host, server_port) and
                connection["remote"] == (client_host, client_port)]
    for connection in matching:
        ownership = adapter.owns(process["pid"], connection)
        identity = socket_identity(connection)
        if registered is not None and registered.get("socket") != identity:
            continue
        if ownership is True:
            return {"source": "process_fd", "socket": identity}
        if (ownership is None and getattr(adapter, "supports_opaque_fd", False) and
                len(matching) == 1 and str(connection.get("inode", "0")).isdigit() and
                int(connection.get("inode", "0")) > 0 and process["uid"] == os.geteuid() and
                process["name"] in ("sshd", "sshd-session")):
            # Initial registration is possible only in a real descendant SSH
            # exec channel. On subsequent claims the private durable record,
            # unchanged process birth, and unchanged kernel transport inode
            # retain that proof even if its helper has since exited.
            live_descendant = witness is not None and any(
                same_process(process, ancestor) for ancestor in ancestors(adapter, witness["pid"]))
            durable_proof = registered is not None and registered.get("source") in (
                "ssh_exec_ancestry_inode", "process_fd")
            if live_descendant or durable_proof:
                return {"source": "ssh_exec_ancestry_inode", "socket": identity}
    return None


def connection_owned(adapter, process, transport):
    return transport_proof(adapter, process, transport) is not None


def find_session(adapter, transport):
    witness = adapter.process(os.getpid())
    for process in ancestors(adapter, os.getpid()):
        if process["name"] not in ("sshd", "sshd-session"):
            continue
        if process["uid"] != os.geteuid():
            continue
        if transport_proof(adapter, process, transport, witness=witness) is not None:
            return process
    failure("ownership_mismatch", "no same-user ancestor SSH session owns this SSH_CONNECTION transport")


def listeners(adapter, host, port):
    return [item for item in adapter.sockets() if item["listening"] and
            item["local"][1] == port and overlaps(item["local"][0], host)]


def port_available(host, port):
    parsed = ip(host)
    family = socket.AF_INET6 if parsed.version == 6 else socket.AF_INET
    with socket.socket(family, socket.SOCK_STREAM) as probe:
        # OpenSSH listeners use SO_REUSEADDR. Match that behavior so TIME_WAIT
        # children of a terminated session do not look like a foreign listener.
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind((str(parsed), port))
            return True
        except OSError as error:
            if error.errno in (errno.EADDRINUSE, errno.EACCES):
                return False
            failure("unsupported", "cannot check requested remote listening address: " + str(error))


def private_directory(path):
    try:
        os.makedirs(path, mode=0o700, exist_ok=True)
        identity = os.lstat(path)
    except PermissionError:
        failure("permission_denied", "cannot create private remote lease directory")
    if not stat.S_ISDIR(identity.st_mode) or identity.st_uid != os.geteuid() or identity.st_mode & 0o077:
        failure("permission_denied", "lease directory must be owned by this user, private (0700), and not a symlink: " + path)


def secure_open(path, flags, mode=0o600):
    try:
        descriptor = os.open(path, flags | getattr(os, "O_NOFOLLOW", 0), mode)
        identity = os.fstat(descriptor)
        if (not stat.S_ISREG(identity.st_mode) or identity.st_uid != os.geteuid() or
                identity.st_mode & 0o077 or identity.st_nlink != 1):
            os.close(descriptor)
            failure("permission_denied", "lease files must be private, owned regular files without hard links")
        return descriptor
    except PermissionError:
        failure("permission_denied", "cannot open private remote lease file")
    except OSError as error:
        if error.errno == errno.ELOOP:
            failure("permission_denied", "refusing a symlink in the remote lease registry")
        raise


class Registry:
    def __init__(self, owner, rule):
        base = os.environ.get("FWM_REMOTE_STATE_DIR")
        if base is None:
            state = os.environ.get("XDG_STATE_HOME", os.path.expanduser("~/.local/state"))
            base = os.path.join(state, "fwm")
        if not os.path.isabs(base):
            failure("unsupported", "remote state directory must be an absolute path")
        private_directory(base)
        leases = os.path.join(base, "leases")
        private_directory(leases)
        self.directory = os.path.join(leases, owner)
        private_directory(self.directory)
        self.path = os.path.join(self.directory, rule + ".json")
        self.lock_path = os.path.join(self.directory, rule + ".lock")
        self.lock = None

    def __enter__(self):
        self.lock = secure_open(self.lock_path, os.O_CREAT | os.O_RDWR)
        fcntl.flock(self.lock, fcntl.LOCK_EX)
        return self

    def __exit__(self, *_):
        os.close(self.lock)
        self.lock = None

    def read(self):
        try:
            descriptor = secure_open(self.path, os.O_RDONLY)
        except FileNotFoundError:
            return None
        with os.fdopen(descriptor, encoding="utf8") as stream:
            text = stream.read(MAX_LINE + 1)
        if len(text) > MAX_LINE:
            failure("ownership_mismatch", "remote lease record is too large")
        try:
            result = json.loads(text)
        except ValueError:
            failure("ownership_mismatch", "remote lease record is malformed")
        if not isinstance(result, dict):
            failure("ownership_mismatch", "remote lease record is malformed")
        return result

    def write(self, record):
        temporary = self.path + ".tmp-" + uuid.uuid4().hex
        descriptor = secure_open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        try:
            with os.fdopen(descriptor, "w", encoding="utf8") as stream:
                json.dump(record, stream, separators=(",", ":"))
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            directory = os.open(self.directory, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    def delete(self):
        os.unlink(self.path)


def validate_claim(command):
    result = {}
    for name in ("owner_id", "rule_id", "session_id"):
        value = command.get(name)
        try:
            normalized = str(uuid.UUID(value))
        except (ValueError, AttributeError, TypeError):
            failure("invalid_request", name + " must be a UUID")
        if value != normalized:
            failure("invalid_request", name + " must be a canonical UUID")
        result[name] = value
    generation = command.get("generation")
    port = command.get("listen_port")
    if type(generation) is not int or not 0 <= generation < 2 ** 64:
        failure("invalid_request", "generation must be an unsigned 64-bit integer")
    if type(port) is not int or not 0 < port <= 65535:
        failure("invalid_request", "listen_port must be in 1..65535")
    result.update(generation=generation, listen_host=str(ip(command.get("listen_host"))), listen_port=port)
    return result


def matches_lease(record, token):
    return isinstance(record, dict) and all(record.get(key) == token.get(key)
                                           for key in ("owner_id", "rule_id", "generation", "session_id"))


class LeaseHelper:
    def __init__(self, adapter=None):
        self.adapter = adapter if adapter is not None else platform_adapter()
        self.current = None
        self.registry = None

    def identify(self):
        transport = ssh_connection(os.environ.get("SSH_CONNECTION", ""))
        session = find_session(self.adapter, transport)
        helper = self.adapter.process(os.getpid())
        if helper is None:
            failure("ownership_mismatch", "cannot identify the current helper process")
        return session, helper, transport

    def check_record(self, old, requested):
        try:
            validated = validate_claim(old)
        except LeaseError:
            failure("ownership_mismatch", "existing remote lease has invalid identity fields")
        if (old.get("protocol") != PROTOCOL or old.get("phase") not in ("claimed", "confirmed") or
                any(validated[key] != requested[key] for key in ("owner_id", "rule_id")) or
                not isinstance(old.get("session"), dict) or not isinstance(old.get("helper"), dict) or
                not isinstance(old.get("transport"), list) or len(old["transport"]) != 4):
            failure("ownership_mismatch", "existing remote lease has an unsupported or mismatched identity")
        for identity in (old["session"], old["helper"]):
            if (type(identity.get("pid")) is not int or identity["pid"] <= 1 or
                    identity.get("uid") != os.geteuid() or not isinstance(identity.get("birth"), str) or
                    not isinstance(identity.get("name"), str)):
                failure("ownership_mismatch", "existing lease process identity is invalid")
        if old["session"]["name"] not in ("sshd", "sshd-session"):
            failure("ownership_mismatch", "existing lease does not identify an SSH session")
        if old["generation"] >= requested["generation"]:
            failure("superseded", "a newer or equal generation already owns this rule")

    def verify_old(self, old):
        session = self.adapter.process(old["session"]["pid"])
        bound = listeners(self.adapter, old["listen_host"], old["listen_port"])
        if not same_process(old["session"], session):
            if bound or not port_available(old["listen_host"], old["listen_port"]):
                failure("unmanaged_conflict", "old SSH identity is gone but the listening port belongs to another process")
            return None
        helper = self.adapter.process(old["helper"]["pid"])
        if helper is not None and not same_process(old["helper"], helper):
            failure("ownership_mismatch", "old helper PID has been reused; refusing the mismatched witness")
        if helper is not None and not any(same_process(old["session"], item) for item in ancestors(self.adapter, helper["pid"])):
            failure("ownership_mismatch", "old helper is not a descendant of the recorded SSH session")
        registered = old.get("session_proof")
        if registered is not None and old.get("registration") != "ssh_exec_ancestry":
            failure("ownership_mismatch", "old session proof was not registered by an SSH exec descendant")
        proof = transport_proof(self.adapter, session, old["transport"], witness=helper, registered=registered)
        if proof is None:
            failure("ownership_mismatch", "old SSH session no longer owns its registered transport connection")
        if helper is None and registered is None:
            failure("ownership_mismatch", "old helper is gone and no durable session identity proof exists")
        ownership = [self.adapter.owns(session["pid"], item) for item in bound]
        if any(value is False for value in ownership):
            failure("unmanaged_conflict", "port ownership does not match the registered old SSH session")
        if any(value is None for value in ownership):
            if (not getattr(self.adapter, "supports_opaque_fd", False) or
                    registered is None or old.get("registration") != "ssh_exec_ancestry"):
                failure("ownership_mismatch", "opaque socket ownership has no registered SSH session proof")
            recorded_listener = old.get("listener_proof")
            if old["phase"] == "confirmed":
                observed = [socket_identity(item) for item in bound]
                if not isinstance(recorded_listener, dict) or recorded_listener.get("sockets") != observed:
                    failure("unmanaged_conflict", "reverse listener inode changed after the SSH-confirmed registration")
            # A claimed session may have opened its reverse listener just before
            # the client lost the confirmation reply. Its saved SSH-exec ancestry,
            # birth identity and transport inode still identify OUR dedicated
            # session, independently of who currently owns the target port.
            # Signal only that registered session, then independently verify the
            # requested port became free; never derive a signal PID from a port.
        if not bound and not port_available(old["listen_host"], old["listen_port"]):
            failure("unmanaged_conflict", "listening socket ownership could not be verified")
        return session

    def terminate_old(self, old):
        session = self.verify_old(old)
        if session is None:
            return False
        if same_process(session, self.current["session"]):
            failure("ownership_mismatch", "old lease identifies the current connection; dedicated connections are required")
        descriptor = self.adapter.open_process(session)
        if descriptor is None:
            return False
        try:
            # Recheck after pinning the process, under the same per-rule lock.
            if self.verify_old(old) is None:
                return False
            self.adapter.signal_process(descriptor, session, signal.SIGTERM)
            if self.wait_gone(session, TERM_SECONDS):
                return True
            # SIGTERM may close the transport/listener before the process exits.
            # The pinned Linux pidfd still designates exactly the same process.
            # macOS rechecks birth identity immediately before each signal.
            if not same_process(session, self.adapter.process(session["pid"])):
                return True
            self.adapter.signal_process(descriptor, session, signal.SIGKILL)
            if not self.wait_gone(session, KILL_SECONDS):
                failure("permission_denied", "old SSH session did not exit after termination")
            return True
        finally:
            self.adapter.close_process(descriptor)

    def wait_gone(self, process, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            current = self.adapter.process(process["pid"])
            if not same_process(process, current) or current.get("state") in ("Z", "5"):
                return True
            time.sleep(0.05)
        return False

    def claim(self, command):
        requested = validate_claim(command)
        if self.current is not None:
            failure("invalid_request", "this helper already claimed a rule")
        session, helper, transport = self.identify()
        proof = transport_proof(self.adapter, session, transport, witness=helper)
        if proof is None:
            failure("ownership_mismatch", "cannot register SSH session transport identity")
        self.current = dict(requested, protocol=PROTOCOL, phase="claimed", session=session,
                            helper=helper, transport=transport, session_proof=proof,
                            registration="ssh_exec_ancestry")
        self.registry = Registry(requested["owner_id"], requested["rule_id"])
        try:
            with self.registry as registry:
                old = registry.read()
                reclaimed = False
                if old is not None:
                    self.check_record(old, requested)
                    # A rule may have changed its requested listening address.
                    # Check the new address before interrupting its verified old
                    # session, so an unrelated conflict cannot cause collateral
                    # interruption of an otherwise usable old forward.
                    if (old["listen_host"], old["listen_port"]) != (requested["listen_host"], requested["listen_port"]):
                        incoming = listeners(self.adapter, requested["listen_host"], requested["listen_port"])
                        if incoming or not port_available(requested["listen_host"], requested["listen_port"]):
                            failure("unmanaged_conflict", "the new listening address is occupied; leaving the old session intact")
                    reclaimed = self.terminate_old(old)
                # Check even when no procfs/lsof entry was visible. bind catches
                # privileged listeners hidden by the system's process policy.
                if listeners(self.adapter, requested["listen_host"], requested["listen_port"]) or not port_available(requested["listen_host"], requested["listen_port"]):
                    failure("unmanaged_conflict", "remote listening port is occupied by an unregistered process")
                self.current["listener_absent_at_claim"] = True
                registry.write(self.current)
            return dict(ok=True, op="claim", protocol=PROTOCOL, reclaimed=reclaimed,
                        session_pid=session["pid"], generation=requested["generation"], session_id=requested["session_id"])
        except Exception:
            self.current = None
            self.registry = None
            raise

    def confirm(self, command=None):
        self.require_claim()
        with self.registry as registry:
            record = registry.read()
            if not matches_lease(record, self.current):
                failure("superseded", "this helper no longer owns the current generation")
            current_process = self.adapter.process(self.current["session"]["pid"])
            if not same_process(self.current["session"], current_process) or transport_proof(
                    self.adapter, current_process, self.current["transport"],
                    witness=self.current["helper"], registered=self.current.get("session_proof")) is None:
                failure("ownership_mismatch", "current SSH process/transport identity changed")
            bound = listeners(self.adapter, record["listen_host"], record["listen_port"])
            if not bound:
                failure("listener_missing", "SSH has not established the registered reverse listener")
            ownership = [self.adapter.owns(current_process["pid"], item) for item in bound]
            if any(value is False for value in ownership):
                failure("unmanaged_conflict", "reverse listener belongs to an unregistered process")
            if any(str(ip(item["local"][0])) != record["listen_host"] for item in bound):
                failure("ownership_mismatch", "sshd changed the requested bind address; check GatewayPorts")
            opaque = any(value is None for value in ownership)
            if opaque:
                if (not getattr(self.adapter, "supports_opaque_fd", False) or
                        command is None or command.get("forward_ack") is not True or
                        record.get("listener_absent_at_claim") is not True or
                        record.get("registration") != "ssh_exec_ancestry"):
                    failure("ownership_mismatch", "opaque listener confirmation requires this SSH connection's forwarding-success acknowledgement")
                if any(item.get("uid") != current_process["uid"] or not str(item.get("inode", "0")).isdigit() or
                       int(item.get("inode", "0")) <= 0 for item in bound):
                    failure("unmanaged_conflict", "new reverse listener UID/inode does not match the authenticated SSH session")
            record["listener_proof"] = {"source": "ssh_forward_ack_inode" if opaque else "process_fd",
                                        "sockets": [socket_identity(item) for item in bound]}
            record["phase"] = "confirmed"
            registry.write(record)
            self.current = record
        return dict(ok=True, op="confirm", protocol=PROTOCOL, generation=record["generation"], session_id=record["session_id"], session_pid=current_process["pid"])

    def release(self):
        self.require_claim()
        with self.registry as registry:
            record = registry.read()
            if not matches_lease(record, self.current):
                failure("superseded", "refusing to remove a newer session's lease")
            registry.delete()
        return dict(ok=True, op="release", protocol=PROTOCOL, generation=self.current["generation"], session_id=self.current["session_id"])

    def require_claim(self):
        if self.current is None or self.registry is None:
            failure("invalid_request", "claim must succeed before this operation")

    def dispatch(self, command):
        if not isinstance(command, dict):
            failure("invalid_request", "request must be a JSON object")
        operation = command.get("op")
        if operation == "claim":
            return self.claim(command)
        if operation == "confirm":
            return self.confirm(command)
        if operation == "release":
            return self.release()
        failure("invalid_request", "unknown operation; expected claim, confirm, or release")


def main():
    helper = None
    while True:
        line = sys.stdin.readline(MAX_LINE + 1)
        if not line:
            return 0
        operation = None
        try:
            if len(line) > MAX_LINE:
                failure("invalid_request", "request exceeds the protocol line limit")
            try:
                command = json.loads(line)
            except ValueError:
                failure("invalid_request", "request is not valid JSON")
            if isinstance(command, dict):
                operation = command.get("op")
            if helper is None:
                helper = LeaseHelper()
            result = helper.dispatch(command)
        except LeaseError as error:
            result = dict(ok=False, op=operation, protocol=PROTOCOL, code=error.code, message=str(error))
        except OSError as error:
            result = dict(ok=False, op=operation, protocol=PROTOCOL, code="permission_denied" if error.errno in (errno.EACCES, errno.EPERM) else "io_error", message=str(error))
        except Exception as error:
            result = dict(ok=False, op=operation, protocol=PROTOCOL, code="ownership_mismatch", message="cannot safely verify lease: " + str(error))
        sys.stdout.write(json.dumps(result, separators=(",", ":")) + "\n")
        sys.stdout.flush()
        if result.get("ok") and operation == "release":
            return 0
        if len(line) > MAX_LINE:
            return 2


if __name__ == "__main__":
    sys.exit(main())
