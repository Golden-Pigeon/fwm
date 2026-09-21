import json,subprocess,tempfile,signal,select,time
from pathlib import Path
BIN=__import__('pathlib').Path(__file__).resolve().parents[3] / 'target/release/fwm'
results=[]
def report(name, **kw):
 d={'case':name,**kw};results.append(d);print(json.dumps(d,ensure_ascii=False),flush=True)
class Fixture:
 def __enter__(self):
  self.tmp=tempfile.TemporaryDirectory(prefix='fwm-audit-state-',dir='/private/tmp');self.root=Path(self.tmp.name)
  return self
 def run(self,*args,ok=True):
  p=subprocess.run([str(BIN),'--config-dir',str(self.root),'--json',*args],capture_output=True,text=True,timeout=15)
  if ok and p.returncode:raise AssertionError((args,p.stderr,p.stdout))
  return p
 def val(self,*args):return json.loads(self.run(*args).stdout)
 def setup(self):
  ssh=self.root/'ssh.conf';ssh.write_text('Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n')
  self.run('server','add','dev','--host','127.0.0.1','--port','1','--user','test','--ssh-config',str(ssh))
 def add(self,name,port,group=None):
  args=['add',name,'--server','dev','--local','--port',str(port),'--disabled']
  if group:args+=['--group',group]
  return self.val(*args)
 def __exit__(self,*unused):
  self.run('daemon','stop',ok=False);self.tmp.cleanup()

with Fixture() as f:
 (f.root/'config.toml').write_text('''schema_version=3
[[servers]]
id="server"
name="dev"
host="127.0.0.1"
port=1
usr="intended-user"
[[forwards]]
id="rule"
name="web"
server_id="server"
kind="local"
listen="127.0.0.1:31992"
target="localhost:8080"
desired_sate="stopped"
''')
 valid=f.val('config','validate');cfg=f.val('config','export');f.run('config','reload')
 report('unknown_fields',valid=valid,actual_user=cfg['servers'][0]['user'],actual_desired=cfg['forwards'][0]['desired_state'],file_after=(f.root/'config.toml').read_text())
with Fixture() as f:
 f.setup();f.add('task',31992,'g');f.run('edit','task','--rename','archived');f.add('new',31993,'task')
 report('historical_name_shadows_current_group',status=[x['name'] for x in f.val('status','task')['forwards']],short_logs=[x['forward_name'] for x in f.val('logs','task')['events']],group_logs=[x['forward_name'] for x in f.val('logs','--group','task')['events']])
 f.run('edit','g','--rename','h')
 report('group_log_forms_differ',short_groups=[x['group'] for x in f.val('logs','h')['events']],explicit_groups=[x['group'] for x in f.val('logs','--group','h')['events']])
with Fixture() as f:
 f.setup();f.add('web',31992);p=subprocess.Popen([str(BIN),'--config-dir',str(f.root),'--json','logs','--server','dev','--follow','--tail','0'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
 try:
  time.sleep(.65);f.run('edit','web','--tgt','8001')
  ready=bool(select.select([p.stdout],[],[],2)[0]);first=json.loads(p.stdout.readline()) if ready else None
  f.run('server','edit','dev','--rename','prod');f.run('edit','web','--tgt','8002')
  after=bool(select.select([p.stdout],[],[],1.4)[0]);one=json.loads(p.stdout.readline()) if after else None
  report('server_follow_rename',before=first,after=one,still_running=p.poll() is None,retained_new=[e['event']['message'] for e in f.val('logs','--server','prod')['events']])
 finally:p.send_signal(signal.SIGINT);p.communicate(timeout=3)
with Fixture() as f:
 f.setup();f.add('web',31992,'g');p=subprocess.Popen([str(BIN),'--config-dir',str(f.root),'--json','status','--group','g','--watch'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
 try:
  first=json.loads(p.stdout.readline());f.run('edit','g','--rename','h');latest=[]
  for _ in range(2): latest.append(json.loads(p.stdout.readline()))
  report('group_watch_rename',before=len(first['forwards']),after=[len(x['forwards']) for x in latest],current=len(f.val('status','--group','h')['forwards']),still_running=p.poll() is None)
 finally:p.send_signal(signal.SIGINT);p.communicate(timeout=3)
Path('/private/tmp/fwm-audit-state-results.json').write_text(json.dumps(results,ensure_ascii=False,indent=2))
