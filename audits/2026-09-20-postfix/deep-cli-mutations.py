#!/usr/bin/env python3
"""Deterministic offline command sequences; every forward remains stopped."""
import copy
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / 'target/debug/fwm'
RESULTS = []


class Fixture:
    def __init__(self, label, initial=None):
        self.label = label
        self.temporary = tempfile.TemporaryDirectory(prefix='fwm-deep-cli-', dir='/private/tmp')
        self.base = Path(self.temporary.name)
        self.directory = self.base / 'instance'
        self.directory.mkdir()
        if initial is not None:
            (self.directory / 'config.toml').write_text(initial)

    def close(self):
        assert not list(self.directory.rglob('*.sock'))
        self.temporary.cleanup()

    def persisted(self):
        return {str(path.relative_to(self.directory)): path.read_bytes()
                for path in self.directory.rglob('*.toml') if path.is_file()}

    def call(self, name, args, error=False):
        assert args[0] in ('add', 'edit', 'server', 'group', 'config', 'down', 'remove', 'status')
        if args[0] == 'add':
            assert '--disabled' in args
        if args[0] == 'server':
            assert args[1] in ('add', 'edit', 'remove', 'list')
        if args[0] == 'config':
            assert args[1] in ('export', 'validate', 'reload')
        before = self.persisted()
        process = subprocess.run([str(BINARY), '--config-dir', str(self.directory), '--json', *args],
                                 cwd=self.base, capture_output=True, text=True, timeout=8)
        assert process.returncode == (2 if error else 0), (name, process.returncode, process.stdout, process.stderr)
        result = json.loads(process.stderr if error else process.stdout)
        record = {'fixture': self.label, 'case': name, 'args': args, 'exit': process.returncode}
        if error:
            assert self.persisted() == before, (name, 'rejected command changed TOML')
            record['error'] = result['error']
            record['toml_unchanged'] = True
        else:
            config = result.get('data', {}).get('config')
            if config is not None:
                assert all(rule['desired_state'] == 'stopped' for rule in config['forwards'])
                expected = config['revision']
                assert result['data']['revision'] == expected
                for path in ('config.toml', 'state/applied.toml'):
                    local = self.directory / path
                    if local.exists():
                        saved = tomllib.loads(local.read_text())
                        # Serialized TOML may omit default empty values; exported
                        # config below checks the complete logical representation.
                        assert saved['revision'] == expected, (name, path, saved['revision'], expected)
                exported = subprocess.run([str(BINARY), '--config-dir', str(self.directory), '--json', 'config', 'export'],
                                          cwd=self.base, capture_output=True, text=True, timeout=8)
                assert exported.returncode == 0, (name, exported.stderr)
                assert json.loads(exported.stdout) == config, (name, 'reply/export mismatch')
                record['revision'] = expected
                record['config'] = config
        record['passed'] = True
        RESULTS.append(record)
        assert not list(self.directory.rglob('*.sock'))
        return result


def rules(result):
    return {rule['name']: rule for rule in result['data']['config']['forwards']}


def profile(result, name):
    return next(p for p in result['data']['config']['servers'] if p['name'] == name)


f = Fixture('group_and_direction_sequences')
try:
    f.call('new_direct_server', ['server', 'add', 'alpha', '--host', 'alpha.invalid'])
    f.call('second_direct_server', ['server', 'add', 'beta', '--host', 'beta.invalid'])
    a = f.call('stopped_batch_deduplicates_ports', ['add', 'batch', '--server', 'alpha', '--local', '--port', '35001-35002,35001', '--disabled'])
    old = rules(a)
    assert list(old) == ['batch-35001', 'batch-35002']
    assert all(r['group'] == 'batch' for r in old.values())
    a = f.call('group_target_edit_preserves_members_and_sources', ['edit', 'batch', '--tgt', '8081'])
    assert all(r['target'] == 'localhost:8081' for r in rules(a).values())
    assert {n: r['id'] for n, r in rules(a).items()} == {n: r['id'] for n, r in old.items()}
    f.call('group_cannot_collapse_listeners', ['edit', 'batch', '--src', '35003'], error=True)
    f.call('group_edit_cannot_expand_members', ['edit', 'batch', '--port', '35003-35004'], error=True)
    a = f.call('group_rename_preserves_member_names_and_ids', ['edit', 'batch', '--rename', 'renamed'])
    assert set(rules(a)) == set(old)
    assert all(r['group'] == 'renamed' for r in rules(a).values())
    f.call('group_rename_and_move_are_exclusive', ['edit', 'renamed', '--rename', 'again', '--group', 'elsewhere'], error=True)
    a = f.call('single_rule_rename_and_move_is_valid', ['edit', 'batch-35001', '--rename', 'single', '--group', 'other'])
    assert rules(a)['single']['group'] == 'other'
    f.call('group_rename_cannot_merge_existing_group', ['edit', 'renamed', '--rename', 'other'], error=True)
    a = f.call('explicit_group_move_merges_members', ['edit', 'renamed', '--group', 'other'])
    assert all(r['group'] == 'other' for r in rules(a).values())
    a = f.call('group_move_to_existing_server_preserves_ids', ['edit', 'other', '--server', 'beta'])
    beta_id = profile(a, 'beta')['id']
    assert all(r['server_id'] == beta_id for r in rules(a).values())
    a = f.call('group_direction_change_sets_remote_defaults', ['edit', 'other', '--remote'])
    assert all((r['kind'], r['connection_mode'], r['remote_cleanup']) == ('remote', 'dedicated', 'verified') for r in rules(a).values())
    a = f.call('cleanup_off_and_explicit_shared_preserve_mapping', ['edit', 'other', '--remote-cleanup', 'off', '--connection-mode', 'shared'])
    assert all((r['connection_mode'], r['remote_cleanup']) == ('shared', 'off') for r in rules(a).values())
    a = f.call('remote_to_local_clears_cleanup', ['edit', 'other', '--local'])
    assert all((r['kind'], r['remote_cleanup']) == ('local', 'off') for r in rules(a).values())
    f.call('local_cannot_enable_remote_cleanup', ['edit', 'other', '--remote-cleanup', 'verified'], error=True)
    a = f.call('ungroup_preserves_members', ['edit', 'other', '--ungroup'])
    assert all(r.get('group') is None for r in rules(a).values())
    a = f.call('rename_server_keeps_forward_server_ids', ['server', 'edit', 'beta', '--rename', 'production'])
    assert profile(a, 'production')['id'] == beta_id
    assert all(r['server_id'] == beta_id for r in rules(a).values())
    f.call('cannot_remove_in_use_server', ['server', 'remove', beta_id], error=True)
    a = f.call('remove_all_by_server_id', ['remove', '--server', beta_id])
    assert a['data']['config']['forwards'] == []
    f.call('remove_now_empty_server_by_id', ['server', 'remove', beta_id])
finally:
    f.close()

f = Fixture('cross_server_remote_group')
try:
    a = f.call('first_remote_member', ['add', 'r1', '--server', 'alpha', '--remote', '--port', '36000', '--group', 'pair', '--disabled'])
    a = f.call('same_remote_port_on_other_server_allowed', ['add', 'r2', '--server', 'beta', '--remote', '--port', '36000', '--group', 'pair', '--disabled'])
    f.call('moving_group_to_one_server_conflicts_atomically', ['edit', 'pair', '--server', 'newalias'], error=True)
    f.call('changing_group_to_local_conflicts_atomically', ['edit', 'pair', '--local'], error=True)
    f.call('member_source_edit_enables_later_group_move', ['edit', 'r2', '--src', '36001'])
    a = f.call('move_group_to_new_alias_is_single_commit', ['edit', 'pair', '--server', 'newalias'])
    new_id = profile(a, 'newalias')['id']
    assert all(r['server_id'] == new_id for r in rules(a).values())
    f.call('former_unused_server_can_be_removed', ['server', 'remove', 'alpha'])
finally:
    f.close()

f = Fixture('ipv6_and_dynamic_patch')
try:
    a = f.call('explicit_ipv6_mapping', ['add', 'web', '--server', 'dev', '--local=[::1]:37000:[2001:db8::1]:8080', '--disabled'])
    original_id = rules(a)['web']['id']
    a = f.call('target_only_preserves_both_hosts', ['edit', 'web', '--tgt', '8081'])
    assert (rules(a)['web']['listen'], rules(a)['web']['target']) == ('[::1]:37000', '[2001:db8::1]:8081')
    a = f.call('source_only_preserves_ipv6_hosts', ['edit', 'web', '--src', '37001'])
    assert (rules(a)['web']['listen'], rules(a)['web']['target']) == ('[::1]:37001', '[2001:db8::1]:8081')
    a = f.call('legacy_three_part_mapping_keeps_existing_bind', ['edit', 'web', '-R', '37002:target.invalid:8082'])
    assert (rules(a)['web']['kind'], rules(a)['web']['listen'], rules(a)['web']['target']) == ('remote', '[::1]:37002', 'target.invalid:8082')
    a = f.call('same_port_patch_keeps_target_host', ['edit', 'web', '--port', '37003'])
    assert rules(a)['web']['target'] == 'target.invalid:37003'
    a = f.call('switch_remote_to_dynamic_removes_target_and_cleanup', ['edit', 'web', '--dynamic', '[::1]:37004'])
    assert rules(a)['web'].get('target') is None
    assert rules(a)['web']['remote_cleanup'] == 'off'
    f.call('dynamic_without_direction_rejects_target_patch', ['edit', 'web', '--tgt', '8080'], error=True)
    f.call('dynamic_bare_local_requires_destination', ['edit', 'web', '--local'], error=True)
    a = f.call('dynamic_to_local_keeps_bind_with_explicit_destination', ['edit', 'web', '--local', '--tgt', '8083'])
    assert (rules(a)['web']['listen'], rules(a)['web']['target']) == ('[::1]:37004', 'localhost:8083')
    assert rules(a)['web']['id'] == original_id
finally:
    f.close()

f = Fixture('numeric_names_and_option_order')
try:
    a = f.call('numeric_rule_name_with_explicit_ports', ['add', '--local', '123', '--server', '456', '--port', '38000', '--disabled'])
    assert '123' in rules(a)
    a = f.call('numeric_edit_name_after_direction_remains_name', ['edit', '--remote', '123', '--rename', '789'])
    assert rules(a)['789']['listen'] == '127.0.0.1:38000'
    a = f.call('separated_scalar_before_later_name_is_spec', ['edit', '--local', '38001', '--server', '456', '789'])
    assert rules(a)['789']['listen'] == '127.0.0.1:38001'
    a = f.call('numeric_name_before_legacy_scalar_spec', ['edit', '789', '-R', '38002'])
    assert rules(a)['789']['listen'] == '127.0.0.1:38002'
    a = f.call('attached_short_spec_with_global_flags', ['edit', '--json', '-L38003:host.invalid:80', '789'])
    assert rules(a)['789']['target'] == 'host.invalid:80'
    f.call('positional_and_named_add_names_are_exclusive', ['add', 'pos', '--name', 'opt', '--server', '456', '--local', '--port', '38004', '--disabled'], error=True)
    f.call('explicit_mapping_and_port_shorthand_are_exclusive', ['edit', '789', '--local=38004:host.invalid:80', '--src', '38005'], error=True)
    f.call('empty_group_rejected_without_mutation', ['edit', '789', '--group', ''], error=True)
    f.call('empty_rename_rejected_without_mutation', ['edit', '789', '--rename', ''], error=True)
    f.call('empty_server_rejected_without_mutation', ['edit', '789', '--server', ''], error=True)
    f.call('empty_port_rejected_without_mutation', ['edit', '789', '--port', ''], error=True)
finally:
    f.close()

f = Fixture('profile_replacement_and_unset')
try:
    a = f.call('profile_with_overrides', ['server', 'add', 'dev', '--host', 'dev.invalid', '--user', 'alice', '--port', '2222', '--identity', 'key-a', '--identity', 'key-b', '--ssh-config', 'ssh.conf', '--known-hosts', 'hosts', '--proxy-jump', 'jump-a,jump-b'])
    before = profile(a, 'dev')
    a = f.call('profile_unrelated_edit_preserves_all_overrides', ['server', 'edit', 'dev', '--rename', 'renamed'])
    after = profile(a, 'renamed')
    assert dict(after, name='dev') == before
    a = f.call('profile_identity_list_replaced_not_appended', ['server', 'edit', 'renamed', '--identity', 'key-c'])
    assert profile(a, 'renamed')['identity_files'] == [str(f.base / 'key-c')]
    a = f.call('profile_switch_to_alias_retains_other_overrides', ['server', 'edit', 'renamed', '--ssh', 'some-alias'])
    assert profile(a, 'renamed')['host'] is None
    assert profile(a, 'renamed')['user'] == 'alice'
    a = f.call('profile_switch_to_host_clears_alias', ['server', 'edit', 'renamed', '--host', 'other.invalid'])
    assert profile(a, 'renamed')['ssh_alias'] is None
    for field, option, value in [('user', '--user', 'bob'), ('port', '--port', '2200'), ('identity', '--identity', 'key-d'), ('ssh-config', '--ssh-config', 'other.conf'), ('known-hosts', '--known-hosts', 'other-hosts'), ('proxy-jump', '--proxy-jump', 'none')]:
        f.call(f'unset_{field}_conflicts_with_explicit_value', ['server', 'edit', 'renamed', '--unset', field, option, value], error=True)
    a = f.call('unset_multiple_fields_and_repeat', ['server', 'edit', 'renamed', '--unset', 'user,port,identity', '--unset', 'ssh-config,known-hosts,proxy-jump'])
    current = profile(a, 'renamed')
    assert all(current[field] is None for field in ('user', 'port', 'ssh_config', 'known_hosts'))
    assert current['identity_files'] == [] and current['proxy_jump'] == []
    for option in ('--host', '--ssh', '--user', '--identity', '--ssh-config', '--known-hosts', '--proxy-jump'):
        f.call(f'empty_profile_{option[2:]}_rejected', ['server', 'edit', 'renamed', option, ''], error=True)
    f.call('zero_server_port_rejected', ['server', 'edit', 'renamed', '--port', '0'], error=True)
    f.call('none_cannot_mix_with_jump', ['server', 'edit', 'renamed', '--proxy-jump', 'none,jump'], error=True)
    f.call('two_profile_address_modes_rejected', ['server', 'edit', 'renamed', '--host', 'h.invalid', '--ssh', 'alias'], error=True)
finally:
    f.close()

manual = '''schema_version=3
[[servers]]
id="server-handwritten"
name="manual"
host="manual.invalid"
[[forwards]]
name="handwritten"
server_id="server-handwritten"
kind="remote"
listen="127.0.0.1:39000"
target="localhost:8000"
desired_state="stopped"
'''
f = Fixture('handwritten_defaults_and_identity', manual)
try:
    first = f.call('initial_handwritten_export', ['config', 'export'])
    second = f.call('repeat_handwritten_export', ['config', 'export'])
    assert first == second
    assert (first['forwards'][0]['connection_mode'], first['forwards'][0]['remote_cleanup']) == ('dedicated', 'verified')
    a = f.call('first_offline_edit_persists_stable_generated_identity', ['edit', 'handwritten', '--rename', 'persisted'])
    assert rules(a)['persisted']['id'] == first['forwards'][0]['id']
    a = f.call('noop_edit_does_not_increment_revision', ['edit', 'persisted'])
    assert a['data']['revision'] == 1
    f.call('saved_stopped_down_by_id', ['down', first['forwards'][0]['id']])
finally:
    f.close()

f = Fixture('name_identity_and_stopped_listener_boundaries')
try:
    a = f.call('create_server_for_id_selection', ['server', 'add', 'dev', '--host', 'dev.invalid'])
    server_id = profile(a, 'dev')['id']
    a = f.call('add_by_existing_server_id_does_not_create_profile', ['add', 'web', '--server', server_id, '--local=0.0.0.0:40000:host.invalid:80', '--disabled'])
    assert len(a['data']['config']['servers']) == 1
    forward_id = rules(a)['web']['id']
    f.call('rule_name_cannot_shadow_existing_rule_id', ['add', '--name', forward_id, '--server', 'otheralias', '--local', '--port', '40001', '--disabled'], error=True)
    f.call('group_name_cannot_shadow_rule_id', ['edit', 'web', '--group', forward_id], error=True)
    f.call('server_name_cannot_shadow_existing_server_id', ['server', 'add', server_id, '--host', 'other.invalid'], error=True)
    a = f.call('stopped_ungrouped_overlapping_alternative_is_valid', ['add', 'socks', '--server', 'dev', '--dynamic', '127.0.0.1:40000', '--disabled'])
    a = f.call('group_first_stopped_member', ['edit', 'web', '--group', 'batch'])
    f.call('joining_overlapping_stopped_local_and_dynamic_group_rejected', ['edit', 'socks', '--group', 'batch'], error=True)
    a = f.call('move_socks_then_join_group', ['edit', 'socks', '--dynamic', '127.0.0.1:40001', '--group', 'batch'])
    assert rules(a)['socks']['group'] == 'batch'
    f.call('new_profile_not_saved_if_group_name_empty', ['add', '--server', 'never-added', '--local', '--port', '40002', '--group', '', '--disabled'], error=True)
    f.call('new_profile_not_saved_if_generated_batch_names_too_long', ['add', '--server', 'never-added', '--local', '--port', '40002-40003', '--name', 'n' * 100, '--disabled'], error=True)
    previous_revision = a['data']['revision']
    a = f.call('same_group_rename_is_noop', ['edit', 'batch', '--rename', 'batch'])
    assert a['data']['revision'] == previous_revision
    a = f.call('edit_by_rule_id_keeps_identity', ['edit', forward_id, '--rename', 'renamed-web'])
    assert rules(a)['renamed-web']['id'] == forward_id
    a = f.call('remove_individual_id_keeps_other_group_member', ['remove', forward_id])
    assert set(rules(a)) == {'socks'}
    a = f.call('remove_last_group_member_dissolves_group', ['remove', '--group', 'batch'])
    assert rules(a) == {}
finally:
    f.close()

f = Fixture('unicode_and_path_equivalence_controls')
try:
    long_server = '服' * 32 + '甲'
    a = f.call('long_unicode_server_auto_names_stay_valid', ['add', '--server', long_server, '--remote', '--port', '41000-41001', '--disabled'])
    assert all(len(r['name'].encode()) <= 100 and len(r['group'].encode()) <= 100 for r in rules(a).values())
    auto_group = next(iter(rules(a).values()))['group']
    a = f.call('auto_group_is_stable_across_subsequent_batches', ['add', '--server', long_server, '--remote', '--port', '41002-41003', '--disabled'])
    assert {r['group'] for r in rules(a).values()} == {auto_group}
    config_file = f.base / 'ssh.conf'
    config_file.write_text('')
    f.call('profile_relative_path_is_saved_against_cli_cwd', ['server', 'add', 'path-profile', '--host', 'path.invalid', '--ssh-config', 'ssh.conf'])
    a = f.call('equivalent_absolute_path_can_reuse_profile', ['add', 'absolute-path', '--server', 'path-profile', '--ssh-config', str(config_file), '--local', '--port', '41100', '--disabled'])
    assert profile(a, 'path-profile')['ssh_config'] == str(config_file)
    f.call('equivalent_dot_relative_path_can_reuse_profile', ['edit', 'absolute-path', '--server', 'path-profile', '--ssh-config', './ssh.conf'])
    f.call('different_config_path_does_not_silently_replace_profile', ['edit', 'absolute-path', '--server', 'path-profile', '--ssh-config', 'other.conf'], error=True)
finally:
    f.close()

destination = Path(__file__).with_name('deep-cli-mutations.json')
destination.write_text(json.dumps({'scope': 'temporary offline stopped-only command sequences', 'passed': len(RESULTS), 'failed': 0, 'cases': RESULTS}, indent=2) + '\n')
print(json.dumps({'passed': len(RESULTS), 'failed': 0, 'evidence': str(destination)}))
