//! Isolated protocol audit: one loopback russh server, ephemeral keys, no OS SSH
//! service, no remote helper, and no real reverse listening ports.
use fwm_core::{engine::Engine, model::*, ssh};
use russh::{server, keys::{PrivateKey, Algorithm, ssh_key::LineEnding}};
use std::{path::PathBuf, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};
use tokio::{net::TcpListener, io::{AsyncReadExt, AsyncWriteExt}};

#[derive(Clone)]
struct Peer { trace: Arc<Mutex<Vec<String>>>, reject_auth: Arc<AtomicBool>, reject_cancel: Arc<AtomicBool>, inject_old_callback: bool }
impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_publickey_offered(&mut self, _: &str, _: &russh::keys::PublicKey) -> Result<server::Auth, Self::Error> {
        self.trace.lock().unwrap().push("auth offered".into());
        Ok(if self.reject_auth.load(Ordering::SeqCst) { server::Auth::reject() } else { server::Auth::Accept })
    }
    async fn auth_publickey(&mut self, _: &str, _: &russh::keys::PublicKey) -> Result<server::Auth, Self::Error> { Ok(server::Auth::Accept) }
    async fn tcpip_forward(&mut self, address: &str, port: &mut u32, session: &mut server::Session) -> Result<bool, Self::Error> {
        self.trace.lock().unwrap().push(format!("forward {address}:{port}"));
        if self.inject_old_callback && address.starts_with("::ffff:") {
            // An existing listener gets traffic while the replacement request
            // is handled, before this request's failure reply is transmitted.
            session.channel_open_forwarded_tcpip("127.0.0.1", *port, "127.0.0.1", 49152)?;
            return Ok(false);
        }
        Ok(true)
    }
    async fn cancel_tcpip_forward(&mut self, address: &str, port: u32, _: &mut server::Session) -> Result<bool, Self::Error> {
        self.trace.lock().unwrap().push(format!("cancel {address}:{port}"));
        Ok(!self.reject_cancel.load(Ordering::SeqCst))
    }
    async fn channel_open_direct_tcpip(&mut self, channel: russh::Channel<server::Msg>, _: &str, _: u32, _: &str, _: u32, reply: server::ChannelOpenHandle, _: &mut server::Session) -> Result<(), Self::Error> {
        reply.accept().await;
        tokio::spawn(async move { let mut stream=channel.into_stream();let mut buf=[0;1024];while let Ok(n)=stream.read(&mut buf).await {if n==0 {break;} if stream.write_all(&buf[..n]).await.is_err(){break;}} });
        Ok(())
    }
}
fn rule(server: &str, id: &str, listen: &str, target_port: u16) -> ForwardSpec {
    ForwardSpec { id:id.into(), name:id.into(), group:None, server_id:server.into(), tunnel:Tunnel::Remote{listen:listen.parse().unwrap(),target:Endpoint{host:"127.0.0.1".into(),port:target_port}},desired_state:DesiredState::Running,connection_mode:ConnectionMode::Shared,remote_cleanup:RemoteCleanup::Off }
}
async fn ready(engine: &Engine, id: &str) {
    for _ in 0..100 {if engine.snapshot().await.iter().any(|s|s.id==id&&s.state==RuntimeState::Established){return;}tokio::time::sleep(Duration::from_millis(20)).await;}
    panic!("not ready: {:?}",engine.snapshot().await);
}
#[tokio::main]
async fn main() {
    let args:Vec<_>=std::env::args().collect();let mode=&args[1];let dir=PathBuf::from(&args[2]);
    let key=PrivateKey::random(&mut russh::keys::key::safe_rng(),Algorithm::Ed25519).unwrap();key.write_openssh_file(dir.join("identity"),LineEnding::LF).unwrap();
    let host_key=PrivateKey::random(&mut russh::keys::key::safe_rng(),Algorithm::Ed25519).unwrap();
    let cfg=Arc::new(server::Config{keys:vec![host_key],auth_rejection_time:Duration::ZERO,auth_rejection_time_initial:Some(Duration::ZERO),..Default::default()});
    let trace=Arc::new(Mutex::new(Vec::<String>::new()));let reject_auth=Arc::new(AtomicBool::new(false));let reject_cancel=Arc::new(AtomicBool::new(false));
    let peer=Peer{trace:trace.clone(),reject_auth:reject_auth.clone(),reject_cancel:reject_cancel.clone(),inject_old_callback:mode=="mapped_cancel"};
    let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
    let accept=tokio::spawn(async move {let mut children=tokio::task::JoinSet::new();loop{tokio::select!{socket=listener.accept()=>{let Ok((socket,_))=socket else{break};let cfg=cfg.clone();let peer=peer.clone();children.spawn(async move{if let Ok(session)=server::run_stream(cfg,socket,peer).await{let _=session.await;}});},_=children.join_next(),if !children.is_empty()=>{}}}});
    std::fs::write(dir.join("ssh_config"),"Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n").unwrap();
    let mut profile=ServerProfile::new("fixture");profile.id="fixture-server".into();profile.host=Some("127.0.0.1".into());profile.port=Some(port);profile.user=Some("fixture".into());profile.identity_files=vec![dir.join("identity")];profile.ssh_config=Some(dir.join("ssh_config"));profile.known_hosts=Some(dir.join("known_hosts"));
    let policy=RetryPolicy{connect_timeout_secs:2,..Default::default()};let info=ssh::inspect_host_key(&profile,&policy).await.unwrap();ssh::trust_host_key(&info,&info.fingerprint).unwrap();
    let mut config=Config{servers:vec![profile.clone()],..Default::default()};config.defaults.retry=policy;
    let mut engine=Engine::new();
    match mode.as_str() {
        "dual_gate"=>{
            config.forwards=vec![rule(&profile.id,"v4","0.0.0.0:35000",1),rule(&profile.id,"v6","[::1]:35000",1)];config.validate().unwrap();engine.reconcile(&config).await.unwrap();ready(&engine,"v4").await;tokio::time::sleep(Duration::from_millis(400)).await;
            println!("initial={}",serde_json::to_string(&engine.snapshot().await).unwrap());
            config.forwards.remove(0);engine.reconcile(&config).await.unwrap();ready(&engine,"v6").await;println!("after_removing_v4={}",serde_json::to_string(&engine.snapshot().await).unwrap());
        },
        "mapped_cancel"=>{
            let target=TcpListener::bind("127.0.0.1:0").await.unwrap();let target_port=target.local_addr().unwrap().port();
            config.forwards=vec![rule(&profile.id,"old","127.0.0.1:35001",1)];engine.reconcile(&config).await.unwrap();ready(&engine,"old").await;
            reject_cancel.store(true,Ordering::SeqCst);config.forwards[0].desired_state=DesiredState::Stopped;config.forwards.push(rule(&profile.id,"new","[::ffff:127.0.0.1]:35001",target_port));config.validate().unwrap();engine.reconcile(&config).await.unwrap();
            let connected=tokio::time::timeout(Duration::from_secs(2),target.accept()).await.is_ok();
            println!("old_callback_reached_new_target={connected}");println!("states={}",serde_json::to_string(&engine.snapshot().await).unwrap());
            reject_cancel.store(false,Ordering::SeqCst);
        },
        "metadata_attention"=>{
            reject_auth.store(true,Ordering::SeqCst);config.forwards=vec![rule(&profile.id,"attention","127.0.0.1:35002",1)];engine.reconcile(&config).await.unwrap();
            for _ in 0..100 {if engine.snapshot().await[0].state==RuntimeState::NeedsAttention{break;}tokio::time::sleep(Duration::from_millis(20)).await;}
            let before=trace.lock().unwrap().len();config.forwards[0].name="renamed-only".into();engine.reconcile(&config).await.unwrap();tokio::time::sleep(Duration::from_millis(400)).await;
            println!("requests_before_rename={before}");println!("requests_after_rename={}",trace.lock().unwrap().len());println!("states={}",serde_json::to_string(&engine.snapshot().await).unwrap());
        },
        "huge_keepalive"=>{
            let panics=Arc::new(Mutex::new(Vec::new()));let observed=panics.clone();std::panic::set_hook(Box::new(move|info|observed.lock().unwrap().push(info.to_string())));
            config.defaults.retry.keepalive_interval_secs=i64::MAX as u64;config.forwards=vec![rule(&profile.id,"huge","127.0.0.1:35003",1)];config.validate().unwrap();engine.reconcile(&config).await.unwrap();tokio::time::sleep(Duration::from_millis(300)).await;
            println!("panics={}",serde_json::to_string(&*panics.lock().unwrap()).unwrap());println!("states={}",serde_json::to_string(&engine.snapshot().await).unwrap());
        },
        _=>panic!("unknown mode"),
    }
    engine.shutdown().await;accept.abort();println!("trace={}",serde_json::to_string(&*trace.lock().unwrap()).unwrap());
}
