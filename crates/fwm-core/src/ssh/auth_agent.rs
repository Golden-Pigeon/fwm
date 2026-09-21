//! Minimal agent protocol fixture: signs actual SSH challenges and records flags.
use russh::keys::{
    signature::Signer,
    ssh_key::{Certificate, HashAlg, PrivateKey, Signature},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

#[derive(Clone, Copy)]
pub enum Behavior {
    Sign,
    Refuse,
    RefuseOnce,
    Malformed,
    Corrupt,
    Disconnect,
}

#[derive(Clone)]
pub struct Identity {
    pub key: Arc<PrivateKey>,
    pub certificate: Option<Certificate>,
    pub behavior: Behavior,
}
impl Identity {
    pub fn key(key: Arc<PrivateKey>) -> Self {
        Self {
            key,
            certificate: None,
            behavior: Behavior::Sign,
        }
    }
    fn blob(&self) -> Vec<u8> {
        self.certificate.as_ref().map_or_else(
            || self.key.public_key().to_bytes().unwrap(),
            |cert| cert.to_bytes().unwrap(),
        )
    }
}

pub struct Agent {
    pub path: PathBuf,
    pub signed: Arc<Mutex<Vec<(usize, u32)>>>,
    task: JoinHandle<()>,
}
impl Agent {
    pub async fn new(directory: &Path, identities: Vec<Identity>, reject_list: bool) -> Self {
        let path = directory.join("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let signed = Arc::new(Mutex::new(vec![]));
        let observed = signed.clone();
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream,_)) = accepted else { break; };
                        let identities=identities.clone(); let signed=observed.clone();
                        clients.spawn(async move { let _ = serve(stream,identities,signed,reject_list).await; });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Self { path, signed, task }
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn string(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u32).to_be_bytes());
    output.extend_from_slice(value);
}
fn read_string<'a>(input: &mut &'a [u8]) -> &'a [u8] {
    let length = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    let result = &input[4..4 + length];
    *input = &input[4 + length..];
    result
}

async fn serve(
    mut stream: UnixStream,
    identities: Vec<Identity>,
    signed: Arc<Mutex<Vec<(usize, u32)>>>,
    reject_list: bool,
) -> std::io::Result<()> {
    loop {
        let length = stream.read_u32().await? as usize;
        assert!(length < 256 * 1024);
        let mut request = vec![0; length];
        stream.read_exact(&mut request).await?;
        let mut response = vec![];
        match request[0] {
            11 if !reject_list => {
                response.push(12);
                response.extend_from_slice(&(identities.len() as u32).to_be_bytes());
                for identity in &identities {
                    string(&mut response, &identity.blob());
                    string(&mut response, b"fixture");
                }
            }
            13 => {
                let mut input = &request[1..];
                let blob = read_string(&mut input);
                let data = read_string(&mut input);
                let flags = u32::from_be_bytes(input[..4].try_into().unwrap());
                let index = identities
                    .iter()
                    .position(|identity| identity.blob() == blob)
                    .unwrap();
                let identity = &identities[index];
                signed.lock().unwrap().push((index, flags));
                match identity.behavior {
                    Behavior::Refuse => response.push(5),
                    Behavior::RefuseOnce
                        if signed
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|(signed_index, _)| *signed_index == index)
                            .count()
                            == 1 =>
                    {
                        response.push(5)
                    }
                    Behavior::Disconnect => return Ok(()),
                    Behavior::Malformed => response.extend_from_slice(&[14, 0, 0, 0, 1, 0]),
                    Behavior::Sign | Behavior::Corrupt | Behavior::RefuseOnce => {
                        let hash = match flags {
                            2 => Some(HashAlg::Sha256),
                            4 => Some(HashAlg::Sha512),
                            _ => None,
                        };
                        let mut signature: Signature =
                            if let Some(rsa) = identity.key.key_data().rsa() {
                                (rsa, hash).try_sign(data).unwrap()
                            } else {
                                identity.key.try_sign(data).unwrap()
                            };
                        if matches!(identity.behavior, Behavior::Corrupt) {
                            signature = Signature::new(
                                signature.algorithm(),
                                vec![0; signature.as_bytes().len()],
                            )
                            .unwrap();
                        }
                        let mut encoded = vec![];
                        string(&mut encoded, signature.algorithm().as_str().as_bytes());
                        string(&mut encoded, signature.as_bytes());
                        response.push(14);
                        string(&mut response, &encoded);
                    }
                }
            }
            _ => response.push(5),
        }
        stream.write_u32(response.len() as u32).await?;
        stream.write_all(&response).await?;
        stream.flush().await?;
    }
}
