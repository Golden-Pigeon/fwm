//! Local certificate signing with the server's negotiated RSA hash algorithm.
use std::sync::Arc;

use russh::keys::{
    agent::AgentIdentity,
    signature::Signer,
    ssh_key::{HashAlg, PrivateKey, Signature},
};

pub(super) struct LocalSigner(pub Arc<PrivateKey>);

impl russh::Signer for LocalSigner {
    type Error = russh::AgentAuthError;

    async fn auth_sign(
        &mut self,
        identity: &AgentIdentity,
        hash: Option<HashAlg>,
        mut message: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        if identity.public_key().key_data() != self.0.public_key().key_data() {
            return Err(russh::keys::Error::InvalidParameters.into());
        }
        let signature: Signature = if let Some(key) = self.0.key_data().rsa() {
            // Only negotiated SHA-2 signatures are permitted for RSA.
            let hash = hash.ok_or(russh::keys::Error::InvalidParameters)?;
            (key, Some(hash)).try_sign(&message)
        } else {
            self.0.try_sign(&message)
        }
        .map_err(|_| russh::keys::Error::InvalidSignature)?;
        let mut encoded = Vec::new();
        string(&mut encoded, signature.algorithm().as_str().as_bytes());
        string(&mut encoded, signature.as_bytes());
        string(&mut message, &encoded);
        Ok(message)
    }
}

fn string(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    output.extend_from_slice(bytes);
}
