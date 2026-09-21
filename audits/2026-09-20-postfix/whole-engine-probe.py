#!/usr/bin/env python3
"""Compile unchanged data-flow logic against a typed in-memory channel adapter."""
import hashlib
import json
import sys
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[2]
AUDIT=Path(__file__).resolve().parent
SOURCE=ROOT/'crates/fwm-core/src/engine'
DEPS=ROOT/'target/debug/deps'
prioritize_cancel='--prioritize-cancel' in sys.argv
with tempfile.TemporaryDirectory(prefix='fwm-whole-flow-',dir='/private/tmp') as temp:
    temp=Path(temp);(temp/'engine').mkdir()
    adaptations={}
    for name in ['state.rs','socks.rs','retry.rs','channels.rs','forward.rs']:
        text=(SOURCE/name).read_text()
        if name in ['forward.rs','channels.rs']:
            text=text.replace('use russh::{ChannelStream, client};','use crate::memory_channel::{ChannelStream, client};')
            adaptations[name]=['Replace only ChannelStream/client type import with in-memory adapter; russh errors stay real']
        if name=='forward.rs':
            text=text.replace('async fn local(', 'pub(super) async fn local(').replace('async fn serve_local(', 'pub(super) async fn serve_local(')
            adaptations[name].append('Expose private local/serve_local to sibling fixture; function bodies unchanged')
            needle='TcpStream::connect((route.target.host.as_str(), route.target.port))'
            assert text.count(needle)==1
            text=text.replace(needle,'crate::memory_channel::connect_target((route.target.host.as_str(), route.target.port))')
            adaptations[name].append('Observe target-connect future polling with one atomic increment, then await the original Tokio TcpStream::connect with unchanged arguments')
            if prioritize_cancel:
                needle='tokio::select! {\n        _ = route.cancel.cancelled() => {},'
                assert text.count(needle)==1
                text=text.replace(needle,'tokio::select! {\n        biased;\n        _ = route.cancel.cancelled() => {},')
                adaptations[name].append('Counterfactual only: prioritize ready route cancellation in temporary serve_remote copy')
        (temp/'engine'/name).write_text(text)
    shutil.copyfile(AUDIT/'whole-engine-probe.rs',temp/'main.rs')
    binary=temp/'probe'
    cmd=['rustc','--edition=2024','--crate-name','fwm_whole_flow_audit',str(temp/'main.rs'),'-L',f'dependency={DEPS}','-o',str(binary),'-A','dead_code']
    libraries={}
    for name in ['fwm_core','anyhow','tokio','tokio_util','rand','russh','serde_json']:
        library=max(DEPS.glob(f'lib{name}-*.rlib'),key=lambda p:p.stat().st_mtime)
        libraries[name]=str(library);cmd+=['--extern',f'{name}={library}']
    build=subprocess.run(cmd,capture_output=True,text=True,cwd=ROOT)
    if build.returncode:raise RuntimeError(build.stderr)
    run=subprocess.run([str(binary)],capture_output=True,text=True,cwd=temp,timeout=30)
    if run.returncode:raise RuntimeError(run.stderr+run.stdout)
    result=json.loads(run.stdout);result['exit_code']=run.returncode;result['stderr']=run.stderr
    result['temporary_source_adaptations']=adaptations;result['libraries']=libraries
    result['source_sha256']={str((SOURCE/name).relative_to(ROOT)):hashlib.sha256((SOURCE/name).read_bytes()).hexdigest() for name in ['state.rs','socks.rs','retry.rs','channels.rs','forward.rs']}
    result['cancel_priority_counterfactual']=prioritize_cancel
    (AUDIT/('whole-engine-cancel-priority.json' if prioritize_cancel else 'whole-engine-result.json')).write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result,indent=2))
