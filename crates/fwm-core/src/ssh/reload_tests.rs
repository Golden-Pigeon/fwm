use super::*;
use crate::{
    engine::Engine,
    model::{
        Config, ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, RuntimeState, Tunnel,
        new_id,
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn rule(server: &ServerProfile, name: &str, target: u16) -> ForwardSpec {
    ForwardSpec {
        id: new_id(),
        name: name.into(),
        group: None,
        server_id: server.id.clone(),
        tunnel: Tunnel::Local {
            listen: format!("127.0.0.1:{}", free_port()).parse().unwrap(),
            target: format!("127.0.0.1:{target}").parse().unwrap(),
        },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    }
}

async fn ready(engine: &Engine) {
    tokio::time::timeout(std::time::Duration::from_secs(6), async {
        loop {
            if engine
                .snapshot()
                .await
                .iter()
                .all(|rule| rule.state == RuntimeState::Established)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn server_restart_and_config_reload_refresh_aliases_without_disrupting_other_servers() {
    let old = Fixture::start().await;
    let replacement = Fixture::start().await;
    let other = Fixture::start().await;
    old.trust().await;
    replacement.trust().await;
    other.trust().await;
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(old.profile.known_hosts.as_ref().unwrap())
        .unwrap()
        .write_all(
            std::fs::read(replacement.profile.known_hosts.as_ref().unwrap())
                .unwrap()
                .as_slice(),
        )
        .unwrap();
    let write_alias = |destination: &Fixture| {
        std::fs::write(old.profile.ssh_config.as_ref().unwrap(), format!(
            "Host alias\n HostName 127.0.0.1\n Port {}\n User developer\n IdentityFile {}\n IdentityAgent none\n GlobalKnownHostsFile none\n",
            destination.profile.port.unwrap(),destination.profile.identity_files[0].display()
        )).unwrap();
    };
    write_alias(&old);
    let mut selected = old.profile.clone();
    selected.name = "selected".into();
    selected.host = None;
    selected.port = None;
    selected.ssh_alias = Some("alias".into());
    selected.identity_files.clear();
    let mut unrelated = other.profile.clone();
    unrelated.name = "unrelated".into();
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = echo.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    let a = rule(&selected, "a", echo_port);
    let b = rule(&unrelated, "b", echo_port);
    let config = Config {
        servers: vec![selected, unrelated],
        forwards: vec![a.clone(), b.clone()],
        ..Config::default()
    };
    let mut engine = Engine::new();
    engine.reconcile(&config).await.unwrap();
    ready(&engine).await;
    let mut stream = TcpStream::connect(b.tunnel.listen()).await.unwrap();
    async fn ping(stream: &mut TcpStream) {
        stream.write_all(b"still alive").await.unwrap();
        let mut bytes = [0; 11];
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            stream.read_exact(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&bytes, b"still alive");
    }
    ping(&mut stream).await;
    write_alias(&replacement);
    engine.restart(std::slice::from_ref(&a.id)).await.unwrap();
    ready(&engine).await;
    engine.reconcile(&config).await.unwrap();
    ready(&engine).await;
    assert_eq!(
        replacement.auth_count.load(Ordering::Relaxed),
        0,
        "ordinary operations must retain unrelated SSH configuration"
    );
    let report = engine.reconnect_server(&config, "selected").await.unwrap();
    assert_eq!(report.affected, std::slice::from_ref(&a.id));
    ready(&engine).await;
    assert_eq!(replacement.auth_count.load(Ordering::Relaxed), 1);
    assert_eq!(other.auth_count.load(Ordering::Relaxed), 1);
    ping(&mut stream).await;
    write_alias(&old);
    engine.refresh_ssh_config(&config).await.unwrap();
    ready(&engine).await;
    assert_eq!(old.auth_count.load(Ordering::Relaxed), 2);
    assert_eq!(other.auth_count.load(Ordering::Relaxed), 1);
    ping(&mut stream).await;
    engine.shutdown().await;
    echo_task.abort();
}
