import json,pathlib,subprocess,tempfile,socket,time,getpass
binary=__import__('pathlib').Path(__file__).resolve().parents[3] / 'target/release/fwm'
with tempfile.TemporaryDirectory(prefix='fwm-trust-ux-',dir='/private/tmp') as root:
    root=pathlib.Path(root); key=root/'host';client=root/'client';known=root/'known';cfg=root/'manager'
    for path in [key,client]: subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(path)],check=True)
    with socket.socket() as s: s.bind(('127.0.0.1',0));port=s.getsockname()[1]
    empty=root/'empty';empty.write_text('Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n')
    sshd=root/'sshd';sshd.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {key}\nPidFile {root}/pid\nAuthorizedKeysFile {client}.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin yes\nAllowTcpForwarding yes\nLogLevel ERROR\n')
    log=(root/'log').open('w');server=subprocess.Popen(['/usr/sbin/sshd','-D','-e','-f',str(sshd)],stdout=log,stderr=log)
    def run(*args):
        r=subprocess.run([str(binary),'--config-dir',str(cfg),'--json',*args],capture_output=True,text=True,timeout=15)
        print(json.dumps({'args':args,'exit':r.returncode,'out':r.stdout.strip(),'err':r.stderr.strip()}),flush=True)
    try:
        for _ in range(50):
            if server.poll() is not None: raise RuntimeError((root/'log').read_text())
            try:
                s=socket.create_connection(('127.0.0.1',port),timeout=.2);s.close();break
            except OSError: time.sleep(.05)
        fp=subprocess.check_output(['ssh-keygen','-lf',str(key)+'.pub'],text=True).split()[1]
        run('server','add','test','--host','127.0.0.1','--port',str(port),'--user',getpass.getuser(),'--known-hosts',str(known),'--ssh-config',str(empty))
        run('server','trust','test','--fingerprint',fp)
        run('server','trust','test')
        alias=root/'jump-config';aliasknown=root/'alias-known';sharedknown=root/'shared-known'
        alias.write_text(f'Host jump\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile {client}\n IdentityAgent none\n UserKnownHostsFile {aliasknown}\n GlobalKnownHostsFile none\nHost target\n HostName 127.0.0.1\n Port 1\n User {getpass.getuser()}\n ProxyJump jump\n IdentityAgent none\n GlobalKnownHostsFile none\n')
        run('server','add','target','--ssh','target','--ssh-config',str(alias),'--known-hosts',str(sharedknown))
        run('server','trust','target')
        run('server','trust','jump','--ssh-config',str(alias),'--fingerprint',fp)
        run('server','trust','target')
        run('daemon','status')
    finally:
        run('daemon','stop');server.terminate();server.wait(timeout=5);log.close()
