//! Production forward/channel/SOCKS code with a memory-only channel interface.
pub use fwm_core::model;
use std::{collections::VecDeque, marker::PhantomData, pin::Pin, sync::{Arc,Mutex,atomic::{AtomicBool,AtomicUsize,Ordering}}, task::{Context,Poll}};
use tokio::io::{AsyncRead,AsyncWrite,DuplexStream,ReadBuf};

mod memory_channel {
    use super::*;
    pub static TARGET_CONNECT_POLLS:AtomicUsize=AtomicUsize::new(0);
    pub async fn connect_target(address:(&str,u16))->std::io::Result<tokio::net::TcpStream>{TARGET_CONNECT_POLLS.fetch_add(1,Ordering::SeqCst);tokio::net::TcpStream::connect(address).await}
    pub mod client { pub struct Msg; }
    pub struct ChannelStream<M> { inner: DuplexStream, _message: PhantomData<M> }
    impl<M> ChannelStream<M> { pub fn new(inner: DuplexStream)->Self { Self{inner,_message:PhantomData} } }
    impl<M:Unpin> AsyncRead for ChannelStream<M> { fn poll_read(self:Pin<&mut Self>,cx:&mut Context<'_>,buf:&mut ReadBuf<'_>)->Poll<std::io::Result<()>> { Pin::new(&mut self.get_mut().inner).poll_read(cx,buf) } }
    impl<M:Unpin> AsyncWrite for ChannelStream<M> {
        fn poll_write(self:Pin<&mut Self>,cx:&mut Context<'_>,buf:&[u8])->Poll<std::io::Result<usize>> { Pin::new(&mut self.get_mut().inner).poll_write(cx,buf) }
        fn poll_flush(self:Pin<&mut Self>,cx:&mut Context<'_>)->Poll<std::io::Result<()>> { Pin::new(&mut self.get_mut().inner).poll_flush(cx) }
        fn poll_shutdown(self:Pin<&mut Self>,cx:&mut Context<'_>)->Poll<std::io::Result<()>> { Pin::new(&mut self.get_mut().inner).poll_shutdown(cx) }
    }
    pub struct Channel(DuplexStream);
    impl Channel { pub fn into_stream(self)->ChannelStream<client::Msg>{ChannelStream::new(self.0)} }
    pub enum Plan { Ready(DuplexStream), Refuse, Delayed(tokio::sync::oneshot::Receiver<DuplexStream>), Pending }
    #[derive(Default)] pub struct Handle {
        pub plans:Mutex<VecDeque<Plan>>, pub closed:AtomicBool,
        pub calls:Mutex<Vec<(String,u32)>>, pub pending:AtomicUsize,
    }
    struct Pending<'a>(&'a AtomicUsize);
    impl Drop for Pending<'_>{fn drop(&mut self){self.0.fetch_sub(1,Ordering::SeqCst);}}
    impl Handle {
        pub fn with(plan:Plan)->Arc<Self>{let h=Arc::new(Self::default()); h.plans.lock().unwrap().push_back(plan);h}
        pub fn is_closed(&self)->bool {self.closed.load(Ordering::SeqCst)}
        pub async fn channel_open_direct_tcpip(&self,host:String,port:u32,_origin:String,_origin_port:u32)->Result<Channel,russh::Error>{
            self.calls.lock().unwrap().push((host,port));
            self.pending.fetch_add(1,Ordering::SeqCst); let _guard=Pending(&self.pending);
            let plan=self.plans.lock().unwrap().pop_front().expect("memory channel plan");
            match plan {
                Plan::Ready(stream)=>Ok(Channel(stream)),
                Plan::Refuse=>Err(russh::Error::ChannelOpenFailure(russh::ChannelOpenFailure::ConnectFailed)),
                Plan::Delayed(receiver)=>Ok(Channel(receiver.await.expect("late memory reply"))),
                Plan::Pending=>std::future::pending().await,
            }
        }
    }
}

mod engine {
    use super::*;
    use std::{collections::HashMap,time::Duration};
    use tokio::{io::{AsyncReadExt,AsyncWriteExt},net::{TcpListener,TcpStream},sync::{Semaphore,oneshot}};
    use tokio_util::sync::CancellationToken;
    use model::{ForwardSpec,ForwardStatus,DesiredState,ConnectionMode,RemoteCleanup,RuntimeState,Tunnel,RetryPolicy,Endpoint};
    mod state;
    mod socks;
    mod retry;
    mod channels;
    mod forward;
    mod connection { pub(super) type SshHandle=std::sync::Arc<crate::memory_channel::Handle>; pub(super) struct SessionControl; }
    mod remote {
        use super::*;
        pub(super) async fn run(_:state::Rule,_:connection::SshHandle,_:forward::RemoteRoutes,_:std::net::SocketAddr,_:Endpoint,_:RetryPolicy,_:CancellationToken,_:connection::SessionControl){panic!("remote registration excluded from this fixture");}
    }
    fn rule()->state::Rule {
        let spec=ForwardSpec{id:"memory".into(),name:"memory".into(),group:None,server_id:"memory-server".into(),tunnel:Tunnel::Dynamic{listen:"127.0.0.1:1080".parse().unwrap()},desired_state:DesiredState::Running,connection_mode:ConnectionMode::Shared,remote_cleanup:RemoteCleanup::Off};
        let status=ForwardStatus{id:spec.id.clone(),name:spec.name.clone(),group:None,server:"memory-server".into(),kind:"dynamic".into(),listen:"127.0.0.1:1080".into(),target:None,desired_state:DesiredState::Running,state:RuntimeState::Starting,retry_count:0,next_retry_unix_ms:None,last_error:None,active_connections:0};
        let statuses=Arc::new(Mutex::new(HashMap::from([(spec.id.clone(),state::StatusEntry{generation:1,connection_key:"memory".into(),spec:spec.clone(),status})])));
        state::Rule{spec,generation:1,statuses,events:state::Events::new()}
    }
    async fn tcp_pair()->(TcpStream,TcpStream){
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client=TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server,_)=listener.accept().await.unwrap(); (client,server)
    }
    async fn wait_calls(handle:&memory_channel::Handle,count:usize){
        tokio::time::timeout(Duration::from_secs(2),async{while handle.calls.lock().unwrap().len()<count{tokio::task::yield_now().await;}}).await.unwrap();
    }
    async fn wait_active(rule:&state::Rule,count:u64){
        tokio::time::timeout(Duration::from_secs(2),async{while rule.statuses.lock().unwrap()["memory"].status.active_connections!=count{tokio::task::yield_now().await;}}).await.unwrap();
    }
    async fn read_eof(stream:&mut (impl AsyncRead+Unpin))->Vec<u8>{let mut data=vec![];tokio::time::timeout(Duration::from_secs(2),stream.read_to_end(&mut data)).await.unwrap().unwrap();data}

    async fn half_close(client_first:bool)->serde_json::Value{
        let (mut client,server)=tcp_pair().await;
        let (channel,mut peer)=tokio::io::duplex(64);
        let handle=memory_channel::Handle::with(memory_channel::Plan::Ready(channel));
        let limit=Arc::new(Semaphore::new(1)); let permit=limit.clone().acquire_owned().await.unwrap();
        let task=tokio::spawn(forward::serve_local(server,"127.0.0.1:12345".parse().unwrap(),handle.clone(),Some("memory.test:8080".parse().unwrap()),Duration::from_secs(1),permit));
        let request=vec![b'q';8192]; let response=vec![b'r';8192];
        if client_first {
            client.write_all(&request).await.unwrap(); client.shutdown().await.unwrap();
            assert_eq!(read_eof(&mut peer).await,request);
            let response_send=response.clone(); let sender=tokio::spawn(async move{peer.write_all(&response_send).await.unwrap();peer.shutdown().await.unwrap();});
            assert_eq!(read_eof(&mut client).await,response);sender.await.unwrap();
        } else {
            let response_send=response.clone();
            let sender=tokio::spawn(async move{peer.write_all(&response_send).await.unwrap();peer.shutdown().await.unwrap();peer});
            assert_eq!(read_eof(&mut client).await,response); peer=sender.await.unwrap();
            client.write_all(&request).await.unwrap();client.shutdown().await.unwrap();
            assert_eq!(read_eof(&mut peer).await,request);
        }
        task.await.unwrap().unwrap();assert_eq!(limit.available_permits(),1);assert!(!handle.is_closed());
        serde_json::json!({"client_half_closes_first":client_first,"request_bytes":8192,"response_bytes":8192,"duplex_capacity":64,"permit_returned":true,"shared_handle_open":true})
    }

    async fn socks_cases()->serde_json::Value{
        let cases:Vec<(&str,Vec<u8>,Option<(&str,u16)>,Option<u8>)>=vec![
            ("ipv4",vec![5,1,0,1,127,0,0,1,0,80],Some(("127.0.0.1",80)),None),
            ("domain",[vec![5,1,0,3,11],b"example.org".to_vec(),vec![1,187]].concat(),Some(("example.org",443)),None),
            ("ipv6",[vec![5,1,0,4],vec![0;15],vec![1,0,80]].concat(),Some(("::1",80)),None),
            ("unsupported_command",vec![5,3,0,1],None,Some(7)),
            ("unsupported_address",vec![5,1,0,9],None,Some(8)),
            ("bad_reserved",vec![5,1,1,1],None,Some(1)),
            ("zero_port",vec![5,1,0,1,127,0,0,1,0,0],None,Some(1)),
            ("empty_domain",vec![5,1,0,3,0],None,Some(8)),
            ("truncated_address",vec![5,1,0,1,127],None,None),
        ];
        let mut result=vec![];
        for (name,request,target,error_code) in cases {
            let (mut client,mut server)=tokio::io::duplex(128);
            let task=tokio::spawn(async move{socks::handshake(&mut server).await});
            // Each byte is a separate write: application fragmentation is valid.
            for byte in [5,2,2,0] {client.write_all(&[byte]).await.unwrap();tokio::task::yield_now().await;}
            let mut selected=[0;2];client.read_exact(&mut selected).await.unwrap();assert_eq!(selected,[5,0]);
            for byte in request{client.write_all(&[byte]).await.unwrap();}
            client.shutdown().await.unwrap();let remaining=read_eof(&mut client).await;
            let parsed=task.await.unwrap();
            if let Some((host,port))=target{let parsed=parsed.unwrap();assert_eq!(parsed.host,host);assert_eq!(parsed.port,port);}else{assert!(parsed.is_err());}
            if let Some(code)=error_code{assert_eq!(remaining.len(),10);assert_eq!(remaining[1],code);}
            result.push(serde_json::json!({"case":name,"method_reply":selected,"request_reply":remaining,"expected_endpoint":target}));
        }
        let (mut client,mut server)=tokio::io::duplex(32);let task=tokio::spawn(async move{socks::handshake(&mut server).await});
        client.write_all(&[5,1,2]).await.unwrap();assert_eq!(read_eof(&mut client).await,[5,255]);assert!(task.await.unwrap().is_err());
        result.push(serde_json::json!({"case":"no_acceptable_method","reply":[5,255]}));
        serde_json::json!(result)
    }

    async fn dynamic_failure(timeout:bool)->serde_json::Value{
        let (mut client,server)=tcp_pair().await;
        let handle=memory_channel::Handle::with(if timeout{memory_channel::Plan::Pending}else{memory_channel::Plan::Refuse});
        let limit=Arc::new(Semaphore::new(1));let permit=limit.clone().acquire_owned().await.unwrap();
        let task=tokio::spawn(forward::serve_local(server,"127.0.0.1:12345".parse().unwrap(),handle.clone(),None,Duration::from_millis(40),permit));
        client.write_all(&[5,1,0,5,1,0,1,127,0,0,1,0,80]).await.unwrap();
        let reply=read_eof(&mut client).await;assert_eq!(reply.len(),12);assert_eq!(&reply[..2],[5,0]);assert_eq!(reply[3],if timeout{4}else{5});
        assert!(task.await.unwrap().is_err());
        let permits_after_client=limit.available_permits();assert_eq!(permits_after_client,if timeout{0}else{1});
        handle.closed.store(true,Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2),async{while limit.available_permits()!=1{tokio::task::yield_now().await;}}).await.unwrap();
        serde_json::json!({"timeout":timeout,"reply":reply,"permits_after_client_ended":permits_after_client,"permits_after_transport_closed":limit.available_permits()})
    }

    async fn dynamic_success_and_incomplete_handshake()->serde_json::Value{
        let (mut client,server)=tcp_pair().await;
        let (channel,mut peer)=tokio::io::duplex(64);let handle=memory_channel::Handle::with(memory_channel::Plan::Ready(channel));
        let limit=Arc::new(Semaphore::new(1));let permit=limit.clone().acquire_owned().await.unwrap();
        let task=tokio::spawn(forward::serve_local(server,"127.0.0.1:12345".parse().unwrap(),handle.clone(),None,Duration::from_secs(1),permit));
        let request=[vec![5,1,0,5,1,0,3,11],b"example.org".to_vec(),vec![1,187],b"pipelined payload".to_vec()].concat();
        client.write_all(&request).await.unwrap();client.shutdown().await.unwrap();
        assert_eq!(read_eof(&mut peer).await,b"pipelined payload");peer.write_all(b"response").await.unwrap();peer.shutdown().await.unwrap();
        let response=read_eof(&mut client).await;assert_eq!(&response[..2],[5,0]);assert_eq!(&response[2..4],[5,0]);assert_eq!(&response[12..],b"response");task.await.unwrap().unwrap();
        assert_eq!(handle.calls.lock().unwrap().as_slice(),[("example.org".into(),443)]);assert_eq!(limit.available_permits(),1);
        let (mut idle_client,idle_server)=tcp_pair().await;let idle_handle=Arc::new(memory_channel::Handle::default());let permit=limit.clone().acquire_owned().await.unwrap();
        let task=tokio::spawn(forward::serve_local(idle_server,"127.0.0.1:12345".parse().unwrap(),idle_handle.clone(),None,Duration::from_millis(40),permit));
        idle_client.write_all(&[5]).await.unwrap();assert!(read_eof(&mut idle_client).await.is_empty());let error=task.await.unwrap().unwrap_err();assert!(error.to_string().contains("handshake timed out"));assert!(idle_handle.calls.lock().unwrap().is_empty());assert_eq!(limit.available_permits(),1);
        serde_json::json!({"remote_hostname_preserved":true,"pipelined_payload_preserved":true,"success_reply_before_payload":true,"success_permit_released":true,"incomplete_handshake_timeout_closes_client":true,"incomplete_handshake_opens_no_channel":true,"handshake_timeout_permit_released":true})
    }

    async fn late_reply()->serde_json::Value{
        let (sender,receiver)=oneshot::channel();let handle=memory_channel::Handle::with(memory_channel::Plan::Delayed(receiver));
        let limit=Arc::new(Semaphore::new(1));let permit=limit.clone().acquire_owned().await.unwrap();
        let receiver=channels::open_direct(handle.clone(),"memory.test:80".parse().unwrap(),"127.0.0.1:12345".parse().unwrap(),permit);
        wait_calls(&handle,1).await;drop(receiver);assert_eq!(limit.available_permits(),0);
        let (stream,mut peer)=tokio::io::duplex(64);sender.send(stream).unwrap();assert!(read_eof(&mut peer).await.is_empty());
        assert_eq!(limit.available_permits(),1);assert!(!handle.is_closed());
        serde_json::json!({"held_until_reply":true,"late_stream_closed":true,"capacity_released":true,"shared_handle_open":true})
    }

    async fn listener_survives_target_failure()->serde_json::Value{
        let port=TcpListener::bind("127.0.0.1:0").await.unwrap();let listen=port.local_addr().unwrap();drop(port);
        let rule=rule();let mut events=rule.events.subscribe();let cancel=CancellationToken::new();
        let (channel,mut peer)=tokio::io::duplex(64);let handle=memory_channel::Handle::with(memory_channel::Plan::Refuse);handle.plans.lock().unwrap().push_back(memory_channel::Plan::Ready(channel));
        let worker=tokio::spawn(forward::local(rule.clone(),handle.clone(),listen,Some("memory.test:80".parse().unwrap()),RetryPolicy::default(),cancel.clone()));
        tokio::time::timeout(Duration::from_secs(2),async{while !events.recv().await.unwrap().message.contains("Established") {}}).await.unwrap();
        let mut first=TcpStream::connect(listen).await.unwrap();assert!(read_eof(&mut first).await.is_empty());wait_active(&rule,0).await;assert!(!worker.is_finished());
        let mut second=TcpStream::connect(listen).await.unwrap();second.write_all(b"after failure").await.unwrap();second.shutdown().await.unwrap();assert_eq!(read_eof(&mut peer).await,b"after failure");peer.write_all(b"still works").await.unwrap();peer.shutdown().await.unwrap();assert_eq!(read_eof(&mut second).await,b"still works");wait_active(&rule,0).await;
        assert_eq!(rule.statuses.lock().unwrap()["memory"].status.state,RuntimeState::Established);assert!(!handle.is_closed());
        cancel.cancel();worker.await.unwrap();assert!(TcpListener::bind(listen).await.is_ok());
        serde_json::json!({"target_failure_did_not_end_listener":true,"next_client_forwarded":true,"shared_handle_open":true,"active_count_zero":true,"listener_released_on_stop":true})
    }

    async fn cancellation_and_pending_restart()->serde_json::Value{
        let port=TcpListener::bind("127.0.0.1:0").await.unwrap();let listen=port.local_addr().unwrap();drop(port);
        let rule=rule();let handle=memory_channel::Handle::with(memory_channel::Plan::Pending);handle.plans.lock().unwrap().push_back(memory_channel::Plan::Pending);
        let mut events=rule.events.subscribe();let first_cancel=CancellationToken::new();
        let worker=tokio::spawn(forward::local(rule.clone(),handle.clone(),listen,Some("memory.test:80".parse().unwrap()),RetryPolicy::default(),first_cancel.clone()));
        tokio::time::timeout(Duration::from_secs(2),async{while !events.recv().await.unwrap().message.contains("Established") {}}).await.unwrap();
        let mut first=TcpStream::connect(listen).await.unwrap();wait_calls(&handle,1).await;wait_active(&rule,1).await;
        first_cancel.cancel();worker.await.unwrap();assert!(read_eof(&mut first).await.is_empty());wait_active(&rule,0).await;
        let pending_after_stop=handle.pending.load(Ordering::SeqCst);assert_eq!(pending_after_stop,1);
        // Same rule and shared transport restart creates a new listener budget.
        {let mut s=rule.statuses.lock().unwrap();let e=s.get_mut("memory").unwrap();e.generation=2;e.status.state=RuntimeState::Starting;}
        let mut next=rule.clone();next.generation=2;let second_cancel=CancellationToken::new();
        let worker=tokio::spawn(forward::local(next.clone(),handle.clone(),listen,Some("memory.test:80".parse().unwrap()),RetryPolicy::default(),second_cancel.clone()));
        tokio::time::timeout(Duration::from_secs(2),async{while !events.recv().await.unwrap().message.contains("Established") {}}).await.unwrap();
        let mut second=TcpStream::connect(listen).await.unwrap();wait_calls(&handle,2).await;
        let pending_after_restart=handle.pending.load(Ordering::SeqCst);assert_eq!(pending_after_restart,2);
        second_cancel.cancel();worker.await.unwrap();assert!(read_eof(&mut second).await.is_empty());wait_active(&next,0).await;
        handle.closed.store(true,Ordering::SeqCst);tokio::time::timeout(Duration::from_secs(2),async{while handle.pending.load(Ordering::SeqCst)>0{tokio::task::yield_now().await;}}).await.unwrap();
        serde_json::json!({"local_clients_closed_on_stop":true,"active_count_zero_after_stop":true,"pending_open_after_first_stop":pending_after_stop,"pending_open_after_restart_and_one_new_client":pending_after_restart,"pending_open_after_transport_closed":handle.pending.load(Ordering::SeqCst),"scope":"only two clients; demonstrates generations coexist, does not claim measured production capacity overrun"})
    }

    async fn remote_target_failure()->serde_json::Value{
        let port=TcpListener::bind("127.0.0.1:0").await.unwrap();let listen=port.local_addr().unwrap();drop(port);
        let rule=rule();rule.update(RuntimeState::Established,None,0,None);let mut events=rule.events.subscribe();
        let (stream,mut peer)=tokio::io::duplex(64);let route=forward::RemoteRoute{rule:rule.clone(),target:Endpoint{host:listen.ip().to_string(),port:listen.port()},cancel:CancellationToken::new(),limit:Arc::new(Semaphore::new(1)),timeout:Duration::from_secs(1)};
        let task=tokio::spawn(forward::serve_remote(memory_channel::ChannelStream::new(stream),route));
        assert!(read_eof(&mut peer).await.is_empty());task.await.unwrap();assert_eq!(rule.statuses.lock().unwrap()["memory"].status.active_connections,0);assert_eq!(rule.statuses.lock().unwrap()["memory"].status.state,RuntimeState::Established);
        let event=events.recv().await.unwrap();assert!(event.message.contains("target connection failed"));
        serde_json::json!({"channel_closed":true,"active_count_released":true,"rule_remains_established":true,"error_event":event.message})
    }

    async fn cancelled_remote_starts_target()->serde_json::Value{
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let listen=listener.local_addr().unwrap();
        let polls_before=memory_channel::TARGET_CONNECT_POLLS.load(Ordering::SeqCst);
        let accepted=Arc::new(AtomicUsize::new(0));let accepts=accepted.clone();let done=CancellationToken::new();let stop=done.clone();
        let acceptor=tokio::spawn(async move{loop{tokio::select!{biased;_ = stop.cancelled()=>return,result=listener.accept()=>{let (socket,_)=result.unwrap();accepts.fetch_add(1,Ordering::SeqCst);drop(socket);}}}});
        let attempts=24;
        for _ in 0..attempts {
            let rule=rule();let cancel=CancellationToken::new();cancel.cancel();
            let (stream,mut peer)=tokio::io::duplex(64);
            let route=forward::RemoteRoute{rule:rule.clone(),target:Endpoint{host:listen.ip().to_string(),port:listen.port()},cancel,limit:Arc::new(Semaphore::new(1)),timeout:Duration::from_secs(1)};
            assert!(route.cancel.is_cancelled());
            forward::serve_remote(memory_channel::ChannelStream::new(stream),route).await;
            assert!(read_eof(&mut peer).await.is_empty());
            assert_eq!(rule.statuses.lock().unwrap()["memory"].status.active_connections,0);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        done.cancel();acceptor.await.unwrap();
        serde_json::json!({"cancel_already_set_before_every_serve_remote":true,"attempts":attempts,"target_connect_futures_polled_after_cancellation":memory_channel::TARGET_CONNECT_POLLS.load(Ordering::SeqCst)-polls_before,"loopback_target_accepts_after_cancellation":accepted.load(Ordering::SeqCst),"application_payload_bytes":0,"active_count_returns_zero":true,"target_acceptor_on_two_worker_runtime":true})
    }

    async fn remote_transfer_and_cancel()->serde_json::Value{
        let mut results=vec![];
        for cancel_during_transfer in [false,true] {
            let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
            let rule=rule();let cancel=CancellationToken::new();let limit=Arc::new(Semaphore::new(1));let permit=limit.clone().acquire_owned().await.unwrap();
            let (stream,mut peer)=tokio::io::duplex(64);let route=forward::RemoteRoute{rule:rule.clone(),target:Endpoint{host:address.ip().to_string(),port:address.port()},cancel:cancel.clone(),limit:limit.clone(),timeout:Duration::from_secs(1)};
            // The exact permit scope used by Handler after accepting a channel.
            let task=tokio::spawn(async move{let _permit=permit;forward::serve_remote(memory_channel::ChannelStream::new(stream),route).await;});
            let (mut target,_)=tokio::time::timeout(Duration::from_secs(2),listener.accept()).await.unwrap().unwrap();wait_active(&rule,1).await;
            if cancel_during_transfer {
                cancel.cancel();assert!(read_eof(&mut peer).await.is_empty());assert!(read_eof(&mut target).await.is_empty());
            } else {
                peer.write_all(b"remote request").await.unwrap();peer.shutdown().await.unwrap();assert_eq!(read_eof(&mut target).await,b"remote request");target.write_all(b"target response").await.unwrap();target.shutdown().await.unwrap();assert_eq!(read_eof(&mut peer).await,b"target response");
            }
            task.await.unwrap();wait_active(&rule,0).await;assert_eq!(limit.available_permits(),1);
            results.push(serde_json::json!({"cancel_during_transfer":cancel_during_transfer,"active_count_zero":true,"permit_returned":true,"both_sides_closed":true}));
        }
        serde_json::json!(results)
    }

    pub async fn run()->serde_json::Value{
        serde_json::json!({"scope":"normal local TCP and memory duplex channel fixtures; production function logic; no SSH, authentication, remote helper, or real remote server",
            "half_close":[half_close(true).await,half_close(false).await],"socks":socks_cases().await,
            "dynamic_failure":[dynamic_failure(false).await,dynamic_failure(true).await],"late_reply":late_reply().await,
            "dynamic_success_and_timeout":dynamic_success_and_incomplete_handshake().await,
            "listener_target_failure":listener_survives_target_failure().await,"cancel_and_restart":cancellation_and_pending_restart().await,
            "remote_target_failure":remote_target_failure().await,"remote_transfer_and_cancel":remote_transfer_and_cancel().await,"cancelled_remote_start":cancelled_remote_starts_target().await})
    }
}
#[tokio::main(flavor="multi_thread",worker_threads=2)]
async fn main(){println!("{}",engine::run().await);}
