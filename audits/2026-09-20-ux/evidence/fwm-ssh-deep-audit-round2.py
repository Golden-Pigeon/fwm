import json,pathlib,subprocess,tempfile,socket,time,getpass,os
binary=__import__('pathlib').Path(__file__).resolve().parents[3] / 'target/release/fwm'
results=[]
with tempfile.TemporaryDirectory(prefix='fwm-ssh-deep-',dir='/private/tmp') as root:
    root=pathlib.Path(root);key=root/'host';client=root/'client';known=root/'known';cfg=root/'manager'
    for path in [key,client]: subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(path)],check=True)
    def free():
        with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
    port=free();local=free();empty=root/'empty';empty.write_text('Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n')
    sshd=root/'sshd';sshd.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {key}\nPidFile {root}/pid\nAuthorizedKeysFile {client}.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin yes\nAllowTcpForwarding yes\nLogLevel ERROR\n')
    log=(root/'log').open('w');server=subprocess.Popen(['/usr/sbin/sshd','-D','-e','-f',str(sshd)],stdout=log,stderr=log)
    agent=None
    def run(label,*args,env=None):
        r=subprocess.run([str(binary),'--config-dir',str(cfg),'--json',*args],env=env,capture_output=True,text=True,timeout=20)
        result={'label':label,'args':args,'exit':r.returncode}
        for n,v in [('out',r.stdout),('err',r.stderr)]:
            if v:
                try:result[n]=json.loads(v)
                except ValueError:result[n]=v.strip()
        results.append(result); print(json.dumps(result),flush=True);return result
    try:
        for _ in range(50):
            if server.poll() is not None:raise RuntimeError((root/'log').read_text())
            try:s=socket.create_connection(('127.0.0.1',port),timeout=.2);s.close();break
            except OSError:time.sleep(.05)
        fp=subprocess.check_output(['ssh-keygen','-lf',str(key)+'.pub'],text=True).split()[1]
        ssh=root/'ssh-config'
        def write(port):ssh.write_text(f'Host dev\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile {client}\n IdentityAgent none\n GlobalKnownHostsFile none\n')
        write(port)
        run('add profile','server','add','dev','--ssh','dev','--ssh-config',str(ssh),'--known-hosts',str(known))
        run('trust','server','trust','dev','--fingerprint',fp)
        run('initial ready','add','web','--server','dev','--local','--src',str(local),'--tgt','1','--wait','--timeout','5s')
        write(1)
        run('check sees new ssh config','server','check','dev')
        run('restart all server rules old ssh retained','restart','--server','dev','--wait','--timeout','5s')
        run('reload config','config','reload')
        run('after reload still established','status','web')
        run('remove web','remove','web');run('stop','daemon','stop')
        agentpath=root/'agent.sock';agent=subprocess.Popen(['ssh-agent','-D','-a',str(agentpath)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for _ in range(100):
            if agentpath.exists():break
            time.sleep(.01)
        goodenv=os.environ.copy();goodenv['SSH_AUTH_SOCK']=str(agentpath)
        subprocess.run(['ssh-add',str(client)],env=goodenv,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        ssh.write_text(f'Host dev\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile none\n GlobalKnownHostsFile none\n')
        run('offline current agent works','server','check','dev',env=goodenv)
        badenv=os.environ.copy();badenv['SSH_AUTH_SOCK']=str(root/'old-dead-agent.sock')
        run('start daemon old env','daemon','start',env=badenv)
        run('same CLI env online stale agent','server','check','dev',env=goodenv)
        run('restart daemon good env','daemon','restart',env=goodenv)
        run('same check restored','server','check','dev',env=goodenv)
        run('stop','daemon','stop')
        ssh.write_text(f'Host dev\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile none\n IdentitiesOnly yes\n GlobalKnownHostsFile none\n')
        run('IdentitiesOnly yes rejects unrelated agent key','server','check','dev',env=goodenv)
        ssh.write_text(ssh.read_text().replace('IdentitiesOnly yes','IdentitiesOnly true'))
        native=subprocess.run(['ssh','-G','-F',str(ssh),'dev'],capture_output=True,text=True,env=goodenv)
        results.append({'label':'OpenSSH rejects invalid IdentitiesOnly true','exit':native.returncode,'err':native.stderr})
        run('IdentitiesOnly true silently allows agent key','server','check','dev',env=goodenv)
        ssh.write_text(f'Host dev\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile {client}.pub\n IdentitiesOnly yes\n UserKnownHostsFile {known}\n GlobalKnownHostsFile none\n')
        native=subprocess.run(['ssh','-F',str(ssh),'-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','dev','true'],capture_output=True,text=True,env=goodenv,timeout=10)
        results.append({'label':'OpenSSH public IdentityFile authenticates from agent','exit':native.returncode,'err':native.stderr})
        run('public IdentityFile cannot select loaded agent key','server','check','dev',env=goodenv)
        ssh.write_text(f'Host dev\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile {root}/missing-private\n IdentityAgent none\n GlobalKnownHostsFile none\n')
        run('explicit missing private path hidden','server','check','dev',env=goodenv)
    finally:
        run('cleanup','daemon','stop')
        if agent:agent.terminate();agent.wait(timeout=5)
        server.terminate();server.wait(timeout=5);log.close()
pathlib.Path('/private/tmp/fwm-ssh-deep-audit-round2-results.json').write_text(json.dumps(results,indent=2))
