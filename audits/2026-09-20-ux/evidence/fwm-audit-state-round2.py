import subprocess,tempfile,json,socket,threading,struct
from pathlib import Path
BIN=str(__import__('pathlib').Path(__file__).resolve().parents[3] / 'target/release/fwm')
results=[]
def save(name,**kw):
 x={'case':name,**kw};results.append(x);print(json.dumps(x,ensure_ascii=False),flush=True)
with tempfile.TemporaryDirectory(prefix='fwm-audit-more-',dir='/private/tmp') as directory:
 root=Path(directory)
 def cli(*args):return subprocess.run([BIN,'--config-dir',str(root),'--json',*args],capture_output=True,text=True,timeout=10)
 (root/'config.toml').write_text('''schema_version=3
[[servers]]
id="dev"
name="dev"
host="127.0.0.1"
port=1
[[forwards]]
id="r"
name="remote"
server_id="dev"
kind="remote"
listen="127.0.0.1:31992"
target="localhost:8080"
desired_state="stopped"
''')
 p=cli('config','validate');cfg=json.loads(cli('config','export').stdout)
 save('remote_config_defaults',validate=p.returncode,cleanup=cfg['forwards'][0]['remote_cleanup'],mode=cfg['forwards'][0]['connection_mode'])
 cli('config','reload');(root/'state/applied.toml').write_text('broken = [')
 p=cli('config','validate');q=cli('config','reload')
 save('valid_candidate_cannot_repair_broken_snapshot',validate_code=p.returncode,reload_code=q.returncode,reload_error=q.stderr.strip())
with tempfile.TemporaryDirectory(prefix='fwm-audit-watch-',dir='/private/tmp') as directory:
 root=Path(directory);(root/'state').mkdir();path=root/'state/daemon.sock';listener=socket.socket(socket.AF_UNIX);listener.bind(str(path));listener.listen(5)
 methods=[]
 def recv(sock,n):
  out=b''
  while len(out)<n:
   b=sock.recv(n-len(out))
   if not b:raise EOFError
   out+=b
  return out
 def serve():
  for _ in range(2):
   conn,_=listener.accept()
   with conn:
    req=json.loads(recv(conn,struct.unpack('>I',recv(conn,4))[0]));methods.append(req['command']['method'])
    if len(methods)==1:
     body=json.dumps({'api_version':1,'request_id':req['request_id'],'ok':True,'data':{},'error':None}).encode();conn.sendall(struct.pack('>I',len(body))+body)
  listener.close()
 thread=threading.Thread(target=serve,daemon=True);thread.start()
 p=subprocess.run([BIN,'--config-dir',directory,'--json','status','--watch'],capture_output=True,text=True,timeout=5);thread.join(1)
 save('watch_exits_on_transient_ipc_disconnect',requests=methods,code=p.returncode,stdout=p.stdout,stderr=p.stderr.strip())
Path('/private/tmp/fwm-audit-state-round2-results.json').write_text(json.dumps(results,ensure_ascii=False,indent=2))
