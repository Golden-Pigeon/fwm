//! Scripted private IPC peer. Each test owns its directory and server task.
use crate::platform::ipc;
use fwm_api::{
    codec::{read_frame, write_frame},
    protocol::{ApiError, Request, Response},
};
use fwm_core::paths::Paths;
use serde_json::Value;
use tokio::{io::AsyncWriteExt, sync::mpsc, task::JoinHandle};

pub enum Reply {
    Data(Value),
    Failure(&'static str),
    MissingError,
    InvalidJson,
    Disconnect,
    Stall,
}
pub struct Peer {
    pub paths: Paths,
    pub received: mpsc::UnboundedReceiver<Request>,
    _directory: tempfile::TempDir,
    task: JoinHandle<()>,
}
impl Peer {
    pub fn new(replies: Vec<Reply>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().into())).unwrap();
        paths.ensure_dirs().unwrap();
        let listener = ipc::bind(&paths).unwrap();
        let (sender, received) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for reply in replies {
                let mut stream = listener.accept().await.unwrap();
                let request: Request = read_frame(&mut stream).await.unwrap().unwrap();
                sender.send(request.clone()).unwrap();
                let response = match reply {
                    Reply::Data(value) => Response::success(request.request_id, value),
                    Reply::Failure(code) => Response::failure(
                        request.request_id,
                        ApiError::new(code, "scripted failure"),
                    ),
                    Reply::MissingError => {
                        let mut r = Response::success(request.request_id, Value::Null);
                        r.ok = false;
                        r
                    }
                    Reply::InvalidJson => {
                        stream.write_all(&[0, 0, 0, 1, b'{']).await.unwrap();
                        continue;
                    }
                    Reply::Disconnect => continue,
                    Reply::Stall => {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                };
                write_frame(&mut stream, &response).await.unwrap();
            }
            // Keep listener alive; a completed script must not make status use
            // the offline fallback during a query-failure test.
            std::future::pending::<()>().await;
        });
        Self {
            paths,
            received,
            _directory: directory,
            task,
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
