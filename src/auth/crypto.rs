//! AES-256-GCM encryption for secrets at rest.
//! Format: `v1:<base64(nonce || ciphertext)>`, with caller-supplied AAD
//! (`"<table>:<row_id>:<column>"`) binding each ciphertext to its location.

use aes_gcm::aead::Nonce;
use aes_gcm::aead::{Aead, Generate, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const VERSION_PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;

/// Associated data binding a ciphertext to its row and column.
pub fn aad(table: &str, row_id: &str, column: &str) -> String {
    format!("{table}:{row_id}:{column}")
}

#[derive(Clone)]
pub struct Cipher {
    inner: Aes256Gcm,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        let key = Key::<Aes256Gcm>::from(*key);
        Self {
            inner: Aes256Gcm::new(&key),
        }
    }

    pub fn encrypt(&self, plaintext: &str, aad: &str) -> Result<String> {
        let nonce = Nonce::<Aes256Gcm>::generate();
        let ciphertext = self
            .inner
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(nonce.as_slice());
        blob.extend_from_slice(&ciphertext);
        Ok(format!("{VERSION_PREFIX}{}", STANDARD.encode(blob)))
    }

    pub fn decrypt(&self, stored: &str, aad: &str) -> Result<String> {
        let Some(encoded) = stored.strip_prefix(VERSION_PREFIX) else {
            bail!("unsupported ciphertext version");
        };
        let blob = STANDARD
            .decode(encoded)
            .context("ciphertext is not valid base64")?;
        if blob.len() < NONCE_LEN {
            bail!("ciphertext too short");
        }
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::<Aes256Gcm>::try_from(nonce).map_err(|_| anyhow!("bad nonce"))?;
        let plaintext = self
            .inner
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("decryption failed (wrong key, AAD, or tampered data)"))?;
        String::from_utf8(plaintext).context("decrypted value is not UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> Cipher {
        Cipher::new(&[7u8; 32])
    }

    #[test]
    fn roundtrip_and_unique_nonces() {
        let c = cipher();
        let a = c.encrypt("s3cret", "env_vars:1:value_enc").unwrap();
        let b = c.encrypt("s3cret", "env_vars:1:value_enc").unwrap();
        assert_ne!(a, b);
        assert_eq!(c.decrypt(&a, "env_vars:1:value_enc").unwrap(), "s3cret");
    }

    #[test]
    fn wrong_aad_fails() {
        let c = cipher();
        let a = c.encrypt("s3cret", "env_vars:1:value_enc").unwrap();
        assert!(c.decrypt(&a, "env_vars:2:value_enc").is_err());
    }

    #[test]
    fn tampering_fails() {
        let c = cipher();
        let a = c.encrypt("s3cret", "x").unwrap();
        let mut blob = STANDARD.decode(&a[3..]).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 1;
        let tampered = format!("v1:{}", STANDARD.encode(blob));
        assert!(c.decrypt(&tampered, "x").is_err());
    }

    #[test]
    fn wrong_key_and_version_fail() {
        let a = cipher().encrypt("s3cret", "x").unwrap();
        assert!(Cipher::new(&[8u8; 32]).decrypt(&a, "x").is_err());
        assert!(cipher().decrypt(&a.replacen("v1:", "v2:", 1), "x").is_err());
    }
}
