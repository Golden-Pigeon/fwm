//! Run the production platform probe, binary upload, and helper protocol over SSH.
use super::*;
use crate::cleanup::{CleanupError, RemoteLease};
use failure_tests::{claim_reply, cleanup, lease_spec};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn claim_and_release(fixture: &mut Fixture) {
    let handle = fixture.handle.clone();
    let context = cleanup(fixture);
    let task = tokio::spawn(async move {
        let mut lease = RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(lease.session_pid(), 123);
        lease.release().await.unwrap();
    });
    let channel = tokio::time::timeout(Duration::from_secs(3), fixture.session_channels.recv())
        .await
        .unwrap()
        .unwrap();
    let mut io = BufReader::new(channel.into_stream());
    for op in ["claim", "release"] {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(3), io.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["op"], op);
        let response = if op == "claim" {
            claim_reply(&request)
        } else {
            json!({"protocol":1,"op":"release","ok":true})
        };
        io.get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
    }
    task.await.unwrap();
    assert!(!fixture.handle.is_closed());
}

async fn startup_error(fixture: &Fixture) -> CleanupError {
    match RemoteLease::claim(
        fixture.handle.clone(),
        &cleanup(fixture),
        &lease_spec(),
        Duration::from_millis(500),
    )
    .await
    {
        Ok(_) => panic!("invalid helper startup accepted"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn linux_probe_and_upload_accept_eof_before_exit_status() {
    let mut behaviour = Behaviour::default();
    behaviour.native.uname.stdout = vec![b"Linux ".to_vec(), b"x86_64\n".to_vec()];
    let native = behaviour.native.clone();
    let mut fixture = Fixture::with_behaviour(behaviour).await;
    claim_and_release(&mut fixture).await;
    let commands = native.commands.lock().unwrap();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0], "uname -s -m");
    assert!(commands[1].starts_with("umask 077;"));
    assert!(commands[2].starts_with("exec '/tmp/.fwm-remote-helper-"));
    assert_eq!(
        native.uploads.lock().unwrap().as_slice(),
        [include_bytes!("../cleanup/native_helper_linux_x86_64").as_slice()]
    );
}

#[tokio::test]
async fn platform_detection_selects_macos_and_windows_helper_artifacts() {
    for platform in ["Darwin arm64", "Darwin x86_64", "Windows_NT AMD64"] {
        let mut behaviour = Behaviour::default();
        let windows = platform.starts_with("Windows");
        if windows {
            behaviour.native.uname = CommandReply {
                stderr: b"'uname' is not recognized as a command\r\n".to_vec(),
                status: Some(1),
                ..Default::default()
            };
            behaviour.native.windows.stdout = vec![format!("{platform}\r\n").into_bytes()];
        } else {
            behaviour.native.uname.stdout = vec![format!("{platform}\n").into_bytes()];
        }
        behaviour.native.uname.eof_before_status = false;
        behaviour.native.upload.eof_before_status = false;
        let native = behaviour.native.clone();
        let mut fixture = Fixture::with_behaviour(behaviour).await;
        claim_and_release(&mut fixture).await;
        let expected: &[u8] = if windows {
            include_bytes!("../cleanup/native_helper_windows_x86_64.exe")
        } else {
            include_bytes!("../cleanup/native_helper_macos_universal")
        };
        assert_eq!(native.uploads.lock().unwrap().as_slice(), [expected]);
        let commands = native.commands.lock().unwrap();
        assert_eq!(commands.len(), if windows { 4 } else { 3 });
        if windows {
            assert_eq!(commands[1], "echo %OS% %PROCESSOR_ARCHITECTURE%");
            assert!(commands[2].contains("[IO.File]::Open("));
            assert!(commands[3].contains("& (Join-Path"));
        }
    }
}

#[tokio::test]
async fn platform_probe_stderr_does_not_pollute_stdout() {
    let mut behaviour = Behaviour::default();
    behaviour.native.uname.stderr = b"warning from remote shell\n".to_vec();
    let native = behaviour.native.clone();
    let mut fixture = Fixture::with_behaviour(behaviour).await;
    claim_and_release(&mut fixture).await;
    assert_eq!(native.commands.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn nonzero_probe_status_cannot_select_a_platform_from_stdout() {
    let mut behaviour = Behaviour::default();
    behaviour.native.uname.status = Some(17);
    let native = behaviour.native.clone();
    let fixture = Fixture::with_behaviour(behaviour).await;
    let error = startup_error(&fixture).await;
    assert!(
        matches!(&error, CleanupError::Remote { code, .. } if code == "unsupported"),
        "{error}"
    );
    assert_eq!(
        native.commands.lock().unwrap().as_slice(),
        ["uname -s -m", "echo %OS% %PROCESSOR_ARCHITECTURE%"]
    );
    assert!(native.uploads.lock().unwrap().is_empty());
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn upload_nonzero_exit_after_eof_prevents_helper_execution() {
    let mut behaviour = Behaviour::default();
    behaviour.native.upload.status = Some(23);
    let native = behaviour.native.clone();
    let fixture = Fixture::with_behaviour(behaviour).await;
    let error = startup_error(&fixture).await;
    assert!(
        matches!(&error, CleanupError::Remote { code, .. } if code == "helper_upload_failed"),
        "{error}"
    );
    assert!(error.to_string().contains("23"), "{error}");
    assert_eq!(native.commands.lock().unwrap().len(), 2);
    assert_eq!(native.uploads.lock().unwrap().len(), 1);
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn probe_close_without_exit_status_is_a_protocol_failure() {
    let mut behaviour = Behaviour::default();
    behaviour.native.uname.status = None;
    let native = behaviour.native.clone();
    let fixture = Fixture::with_behaviour(behaviour).await;
    let error = startup_error(&fixture).await;
    assert!(matches!(error, CleanupError::Protocol(_)), "{error}");
    assert_eq!(native.commands.lock().unwrap().len(), 1);
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn probe_output_limit_applies_to_stdout_and_stderr() {
    for stderr in [false, true] {
        let mut behaviour = Behaviour::default();
        if stderr {
            behaviour.native.uname.stderr = vec![b'x'; 32769];
        } else {
            behaviour.native.uname.stdout = vec![vec![b'x'; 32769]];
        }
        let fixture = Fixture::with_behaviour(behaviour).await;
        let error = startup_error(&fixture).await;
        assert!(matches!(error, CleanupError::Protocol(_)), "{error}");
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert!(!fixture.handle.is_closed());
    }
}

#[tokio::test]
async fn probe_eof_without_status_or_close_times_out() {
    let mut behaviour = Behaviour::default();
    behaviour.native.uname.status = None;
    behaviour.native.uname.close = false;
    let native = behaviour.native.clone();
    let fixture = Fixture::with_behaviour(behaviour).await;
    let error = startup_error(&fixture).await;
    assert!(matches!(error, CleanupError::Timeout { .. }), "{error}");
    assert_eq!(native.commands.lock().unwrap().len(), 1);
    assert!(!fixture.handle.is_closed());
}
