"""Exercise direct SSH-alias creation using the real loopback SSH fixture."""
import socket
import json


async def check_direct_alias(cli, rpc, echo_test, root, proxy_port, user, identity, known_hosts, target_port):
    ssh_config = root / "direct_alias_config"
    ssh_config.write_text(f'''Host direct-test
  HostName 127.0.0.1
  User {user}
  Port {proxy_port}
  IdentityFile "{identity}"
  UserKnownHostsFile "{known_hosts}"
  GlobalKnownHostsFile none
  IdentityAgent none
''')
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        source_port = listener.getsockname()[1]
    before = (await rpc("get_config"))["data"]
    await cli("server", "check", "direct-test", "--ssh-config", str(ssh_config))
    await cli("doctor", "--server", "direct-test", "--ssh-config", str(ssh_config))
    assert (await rpc("get_config"))["data"] == before, "checking an alias must not register it"
    _, output, _ = await cli("--json", "add", "--server", "direct-test", "--ssh-config", str(ssh_config),
              "--remote", "--src", str(source_port), "--tgt", str(target_port),
              "--wait", "--timeout", "15s")
    result = json.loads(output)
    assert result["ok"] and result["data"]["ready"] and result["data"]["saved"]
    after = (await rpc("get_config"))["data"]
    assert after["revision"] == before["revision"] + 1
    server = next(server for server in after["servers"] if server["name"] == "direct-test")
    name = f"direct-test-remote-{source_port}"
    rule = next(rule for rule in after["forwards"] if rule["name"] == name)
    assert server["ssh_alias"] == "direct-test"
    assert rule["server_id"] == server["id"]
    assert rule["target"] == f"localhost:{target_port}"
    await echo_test(source_port, b"direct-alias-without-registration-or-name")
    await cli("remove", name)
    await cli("server", "remove", "direct-test")
    print("PASS direct SSH alias, automatic naming and atomic server/rule creation", flush=True)
