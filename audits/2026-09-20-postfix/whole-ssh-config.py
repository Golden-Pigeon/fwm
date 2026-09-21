#!/usr/bin/env python3
"""Fixed read-only config cases. Native ssh -G prints config and never connects."""
import json
from pathlib import Path
import subprocess
import tempfile

base = Path(__file__).resolve().parent
root = base.parents[1]
deps = root / 'target/debug/deps'
results = []
cases = {
    'jump_chain_second_reuses_first': '''Host audit
 HostName destination.invalid
 ProxyJump first,second
Host first
 HostName first.invalid
Host second
 HostName second.invalid
 ProxyJump first
''',
    'jump_chain_overrides_second_alias_jump': '''Host audit
 HostName destination.invalid
 ProxyJump first,second
Host first
 HostName first.invalid
Host second
 HostName second.invalid
 ProxyJump extra
Host extra
 HostName extra.invalid
''',
    'jump_uri_user': '''Host audit
 HostName destination.invalid
 ProxyJump ssh://fixture@first.invalid:2201
''',
    'jump_uri_no_user': '''Host audit
 HostName destination.invalid
 ProxyJump ssh://first.invalid:2201
''',
    'identity_hash_inside_word': '''Host audit
 HostName destination.invalid
 IdentityFile /tmp/fixture-key#suffix
''',
    'identity_hash_quoted': '''Host audit
 HostName destination.invalid
 IdentityFile "/tmp/fixture-key#suffix"
''',
    'host_inline_comment': '''Host audit # ordinary comment
 HostName destination.invalid
 Port 2201 # ordinary comment
''',
    'two_identity_arguments': '''Host audit
 HostName destination.invalid
 IdentityFile /tmp/first /tmp/second
''',
    'nested_jump_only': '''Host audit
 HostName destination.invalid
 ProxyJump second
Host second
 HostName second.invalid
 ProxyJump first
Host first
 HostName first.invalid
''',
    'plain_jump_chain': '''Host audit
 HostName destination.invalid
 ProxyJump first,second
Host first
 HostName first.invalid
Host second
 HostName second.invalid
''',
    'scoped_literal_alias': '''Host *
 Port 2201
''',
    'scoped_explicit_host': '''Host *
 Port 2201
''',
    'ipv6_without_scope': '''Host *
 Port 2201
''',
    'hostname_template': '''Host audit
 HostName %h.example.invalid
''',
}
with tempfile.TemporaryDirectory(prefix='fwm-whole-config-') as temporary:
    directory = Path(temporary)
    binary = directory / 'resolve'
    command = ['rustc', '--edition=2024', str(base/'whole-resolve.rs'), '-L', f'dependency={deps}', '-o', str(binary)]
    for name in ['fwm_core','serde_json']:
        library = max(deps.glob(f'lib{name}-*.rlib'), key=lambda path: path.stat().st_mtime)
        command += ['--extern', f'{name}={library}']
    build = subprocess.run(command, capture_output=True, text=True, timeout=60)
    assert build.returncode == 0, build.stderr
    for name, content in cases.items():
        path = directory / f'{name}.conf'
        text = content + f'Host *\n User fixture\n IdentityAgent none\n GlobalKnownHostsFile none\n UserKnownHostsFile {directory}/unused_known_hosts\n'
        path.write_text(text)
        alias = {'scoped_literal_alias':'fe80::1%lo0','scoped_explicit_host':'fe80::1%lo0', 'ipv6_without_scope':'::1'}.get(name,'audit')
        native = subprocess.run(['ssh','-vvv','-G','-F',str(path),alias], capture_output=True, text=True, timeout=10)
        fwm = subprocess.run([str(binary),alias,str(path), 'host' if name == 'scoped_explicit_host' else 'alias'],capture_output=True,text=True,timeout=10)
        results.append({'case':name,'config':text,'native_exit':native.returncode,
            'native_config':[line for line in native.stdout.splitlines() if line.split(' ',1)[0] in {'hostname','user','port','proxyjump','identityfile'}],
            'native_debug':[line for line in native.stderr.splitlines() if 'proxy' in line.lower() or 'extra' in line.lower() or 'bad' in line.lower()],
            'fwm_exit':fwm.returncode,'fwm':json.loads(fwm.stdout)})
    # The generated native command for explicit chain puts -J first on second,
    # so compare that command's configuration without executing the transport.
    path = directory / 'jump_chain_overrides_second_alias_jump.conf'
    native = subprocess.run(['ssh','-G','-F',str(path),'-J','first','second'],capture_output=True,text=True,timeout=10)
    results.append({'case':'native_explicit_second_hop','native_exit':native.returncode,
                    'native_config':[line for line in native.stdout.splitlines() if line.startswith(('hostname ','proxyjump '))]})
by_case = {row['case']:row for row in results}
def route(case): return [item['alias'] for item in by_case[case]['fwm']]
assert route('jump_chain_overrides_second_alias_jump') == ['first','extra','second','audit']
assert route('jump_chain_second_reuses_first') == ['first','first','second','audit']
assert 'proxyjump first' in by_case['native_explicit_second_hop']['native_config']
assert by_case['jump_uri_user']['fwm'][0]['user'] == 'ssh://fixture'
assert 'proxyjump fixture@first.invalid:2201' in by_case['jump_uri_user']['native_config']
assert by_case['jump_uri_no_user']['fwm_exit'] == 2
assert by_case['identity_hash_inside_word']['fwm'][0]['identity_files'][0].endswith('/fixture-key')
assert by_case['identity_hash_quoted']['fwm'][0]['identity_files'][0].endswith('/fixture-key#suffix')
assert by_case['two_identity_arguments']['native_exit'] != 0 and by_case['two_identity_arguments']['fwm_exit'] == 0
assert route('plain_jump_chain') == route('nested_jump_only') == ['first','second','audit']
for case in ['scoped_literal_alias','scoped_explicit_host']:
    assert by_case[case]['native_exit'] == 0 and by_case[case]['fwm_exit'] == 2
    assert 'hostname fe80::1%lo0' in by_case[case]['native_config']
assert by_case['ipv6_without_scope']['fwm'][0]['host'] == '::1'
assert by_case['hostname_template']['fwm'][0]['host'] == 'audit.example.invalid'
assert by_case['host_inline_comment']['fwm'][0]['port'] == 2201
native_version = subprocess.run(['ssh','-V'],capture_output=True,text=True,timeout=5)
(base/'whole-ssh-config.json').write_text(json.dumps({'scope':'read-only fwm resolver and ssh -G only; temporary explicit configs; no connection or credential reads','assertions_passed':True,'native_version':native_version.stderr.strip(),'cases':results},ensure_ascii=False,indent=2)+'\n')
print(json.dumps([{'case':case['case'],'native_exit':case['native_exit'],'fwm_exit':case.get('fwm_exit'),
                  'route':[(item['host'],item['user'],item['port']) for item in case['fwm']] if isinstance(case.get('fwm'),list) else case.get('fwm'),
                  'native_debug':case.get('native_debug',[])} for case in results],ensure_ascii=False,indent=2))
