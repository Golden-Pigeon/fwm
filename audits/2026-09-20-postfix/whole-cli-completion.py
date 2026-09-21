#!/usr/bin/env python3
"""Compile direct source imports, then run deterministic memory-only cases."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DEPS = ROOT / 'target/debug/deps'
CASES = [
    'port_replaced_during_wait', 'batch_member_port_replaced', 'server_replaced_during_wait',
    'kind_replaced_during_wait', 'server_profile_replaced_same_display', 'rename_only_control', 'unrelated_revision_control',
    'batch_partial_then_all_ready', 'batch_partial_timeout', 'batch_attention_fails', 'batch_added_group_member_not_selected',
    'selected_deleted_control', 'selected_stopped_control', 'same_name_new_id_control',
    'unselected_attention_ignored', 'restart_instance_control', 'request_stall_deadline',
    'request_slow_success_deadline', 'request_failure_preserves_last',
    'startup_failure_reports_saved', 'startup_delay_outside_wait_budget', 'no_wait_does_not_query_runtime',
]
results = []
with tempfile.TemporaryDirectory(prefix='fwm-whole-cli-', dir='/private/tmp') as directory:
    binary = Path(directory) / 'memory-completion'
    # Rust's #[path] external module changes the child-module lookup base.
    # Stage byte-identical configuration sources with the expected child name.
    original_configuration = ROOT / 'crates/fwm/src/configuration.rs'
    staged_configuration = Path(directory) / 'configuration.rs'
    shutil.copyfile(original_configuration, staged_configuration)
    shutil.copyfile(ROOT / 'crates/fwm/src/configuration/batch.rs', Path(directory) / 'batch.rs')
    source = (HERE / 'whole-cli-completion.rs').read_text().replace(str(original_configuration), str(staged_configuration))
    staged_probe = Path(directory) / 'probe.rs'
    staged_probe.write_text(source)
    command = ['rustc', '--edition=2024', '-A', 'dead_code', str(staged_probe), '-L', f'dependency={DEPS}', '-o', str(binary)]
    for dependency in ('anyhow', 'fwm_core', 'fwm_api', 'serde', 'serde_json', 'tokio', 'clap'):
        library = max(DEPS.glob(f'lib{dependency}-*.rlib'), key=lambda path: path.stat().st_mtime_ns)
        command += ['--extern', f'{dependency}={library}']
    environment = dict(os.environ, CARGO_PKG_VERSION='0.1.0')
    compilation = subprocess.run(command, env=environment, capture_output=True, text=True)
    assert compilation.returncode == 0, compilation.stderr
    for case in CASES:
        output = subprocess.run([str(binary), case], capture_output=True, text=True, timeout=8)
        assert output.returncode == 0, (case, output.stderr, output.stdout)
        values = [json.loads(line) for line in output.stdout.splitlines()]
        assert len(values) == 2, (case, values)
        response, trace = values
        assert trace['audit_case'] == case
        if case in ('port_replaced_during_wait', 'batch_member_port_replaced', 'server_replaced_during_wait', 'kind_replaced_during_wait', 'server_profile_replaced_same_display'):
            assert response['ok'] and response['data']['ready']
            assert response['data']['revision'] == 7
            assert response['data']['runtime']['config_revision'] == 8
        if case == 'no_wait_does_not_query_runtime':
            assert trace['status_queries'] == 0 and response['data']['ready'] is None
        if case in ('request_stall_deadline', 'request_slow_success_deadline', 'batch_partial_timeout', 'selected_deleted_control', 'selected_stopped_control', 'same_name_new_id_control'):
            assert 350 <= trace['elapsed_virtual_ms'] <= 351, (case, trace['elapsed_virtual_ms'])
        results.append({'case': case, 'response': response, 'trace': trace, 'passed': True})
    parser_output = subprocess.run([str(binary), 'parser_contracts'], capture_output=True, text=True, timeout=8)
    assert parser_output.returncode == 0, parser_output.stderr
    parser_cases = json.loads(parser_output.stdout)['parser_cases']
result = {'scope': 'production completion/output/configuration code with a memory-only client and paused Tokio clock; no sockets or processes launched by product code', 'passed': len(results) + len(parser_cases), 'completion_cases': len(results), 'parser_cases': parser_cases, 'cases': results}
(HERE / 'whole-cli-completion.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({'passed': result['passed'], 'completion_cases': len(results), 'parser_cases': len(parser_cases), 'evidence': str(HERE / 'whole-cli-completion.json')}))
