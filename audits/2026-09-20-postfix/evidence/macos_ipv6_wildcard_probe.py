"""Read only this process's temporary IPv6 socket with lsof; no SSH or signals."""
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys

if sys.platform != "darwin":
    raise SystemExit("This lsof/libproc compatibility probe requires macOS")

repo = Path(__file__).resolve().parents[3]
source = repo / "crates/fwm-core/src/cleanup/remote_helper.py"
spec = importlib.util.spec_from_file_location("fwm_audit_helper", source)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)

with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as listener:
    listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
    listener.bind(("::", 0))
    listener.listen(1)
    port = listener.getsockname()[1]
    arguments = ["/usr/sbin/lsof", "-nP", "-a", "-p", str(os.getpid()),
                 f"-iTCP:{port}", "-F", "pfnT"]
    observed = subprocess.run(arguments, capture_output=True, text=True, timeout=10, check=True)
    parsed = [item for item in helper.MacPlatform.parse_lsof(observed.stdout)
              if item["listening"] and item["local"][1] == port]
    result = {
        "family": "AF_INET6", "ipv6_v6only": 1,
        "actual_sockname": listener.getsockname(),
        "lsof_argv": arguments, "lsof_stdout": observed.stdout,
        "lsof_stderr": observed.stderr, "parsed": parsed,
        "requested_listen_host": "::",
        "confirm_address_comparison": [str(helper.ip(item["local"][0])) == "::" for item in parsed],
        "remote_commands_executed": False, "signals_sent": False,
        "tcp_connections_accepted": 0,
    }

Path(__file__).with_name("macos-ipv6-wildcard.json").write_text(
    json.dumps(result, ensure_ascii=False, indent=2)
)
print(json.dumps(result, ensure_ascii=False))
