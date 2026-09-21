import json,tempfile,subprocess
from pathlib import Path
BIN=str(__import__('pathlib').Path(__file__).resolve().parents[3] / 'target/release/fwm')
checks=[]
with tempfile.TemporaryDirectory(prefix='fwm-audit-state-last-',dir='/private/tmp') as d:
 root=Path(d)
 def call(*args,ok=True):
  p=subprocess.run([BIN,'--config-dir',d,'--json',*args],capture_output=True,text=True,timeout=10)
  if ok:assert p.returncode==0,(args,p.stderr)
  return p
 def data(*args):return json.loads(call(*args).stdout)
 try:
  for args in [('status',),('logs',),('server','list'),('config','export'),('daemon','status')]:
   data(*args)
  assert list(root.iterdir())==[];checks.append('fresh read-only commands do not initialize state')
  call('server','add','dev','--host','127.0.0.1','--port','1','--user','fixture')
  call('add','web','--server','dev','--local','--port','33001','--disabled')
  applied=data('config','export');rule=applied['forwards'][0];rule_id=rule['id']
  candidate=(root/'config.toml').read_text().replace('target = "localhost:33001"','target = "localhost:8081"')
  (root/'config.toml').write_text(candidate)
  assert data('config','validate')['valid']
  assert data('config','export')==applied;checks.append('candidate and applied remain distinct until reload')
  p=call('edit','web','--rename','changed',ok=False);assert p.returncode!=0 and 'config_pending_edits' in p.stderr
  assert (root/'config.toml').read_text()==candidate;checks.append('ordinary edits preserve pending draft bytes')
  call('down','web');assert (root/'config.toml').read_text()==candidate
  call('config','reload');cfg=data('config','export')
  assert cfg['forwards'][0]['id']==rule_id and cfg['forwards'][0]['target']=='localhost:8081' and cfg['forwards'][0]['desired_state']=='stopped'
  checks.append('reload applies unrelated draft changes and preserves control intent and IDs')
  assert data('daemon','status')['daemon_running'] is False
  (root/'config.toml').write_text('malformed = [')
  status=data('status');assert status['warnings'] and status['forwards'][0]['id']==rule_id
  p=call('config','validate',ok=False);assert p.returncode!=0 and json.loads(p.stderr)['ok'] is False
  checks.append('broken draft yields failure JSON and offline status warning with last applied state')
  events=root/'state/events.jsonl'
  with events.open('a') as f:f.write('{malformed record}\n')
  log=data('logs','--tail','2');assert log['warnings'] and len(log['events'])<=2
  checks.append('damaged log records warn while retaining valid bounded history')
  call('daemon','start');status=data('status');assert status['daemon_running'] and status['forwards'][0]['desired_state']=='stopped'
  call('daemon','stop');assert data('status')['forwards'][0]['id']==rule_id
  checks.append('daemon lifecycle preserves stopped rule ID and applied configuration despite bad draft')
  call('remove','web');assert data('config','export')['forwards']==[]
  assert data('logs',rule_id)['events'];checks.append('deleted rule remains queryable by stable ID')
 finally:call('daemon','stop',ok=False)
result={'round':4,'new_independent_issues':0,'checked':checks,'known_issues_referenced':['unknown_fields','remote_config_defaults','historical_name_shadows_current_group','group_log_forms_differ','server_follow_rename','group_watch_rename','valid_candidate_cannot_repair_broken_snapshot','watch_exits_on_transient_ipc_disconnect','server_events_unfilterable'],'source_recheck':'core store/model/history, CLI config/status/logs/selection/output, daemon state/dispatch; other command families covered by independent reviewers'}
Path('/private/tmp/fwm-audit-state-round4-results.json').write_text(json.dumps(result,ensure_ascii=False,indent=2));print(json.dumps(result,ensure_ascii=False,indent=2))
