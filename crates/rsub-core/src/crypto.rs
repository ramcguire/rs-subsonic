//! Secret handling: passwords and backend tokens are encrypted at rest with
//! XChaCha20-Poly1305 because Subsonic token auth needs the plaintext password.
//! API keys are only ever stored hashed.

use std::fs;
use std::io;
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use md5::Md5;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("secret key must be {KEY_LEN} bytes (64 hex chars)")]
    BadKey,
    #[error("decryption failed (wrong server key or corrupted data)")]
    Decrypt,
    #[error("system random source failed")]
    Random,
    #[error("key file: {0}")]
    Io(#[from] io::Error),
}

/// What a ciphertext is for. Used as associated data so a ciphertext cannot be
/// swapped between purposes or users.
#[derive(Debug, Clone, Copy)]
pub enum Purpose {
    KeyCheck,
    Password,
    BackendToken,
}

impl Purpose {
    fn tag(self) -> &'static [u8] {
        match self {
            Purpose::KeyCheck => b"keycheck",
            Purpose::Password => b"password",
            Purpose::BackendToken => b"backend-token",
        }
    }
}

pub struct SecretBox {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(..)")
    }
}

impl SecretBox {
    pub fn new(key: &[u8]) -> Result<Self, CryptoError> {
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoError::BadKey)?;
        Ok(SecretBox { cipher })
    }

    /// Load the key from `env_hex` (if set), else from `path`, else generate and
    /// write a new key to `path`.
    pub fn load_or_create(env_hex: Option<&str>, path: &Path) -> Result<Self, CryptoError> {
        if let Some(h) = env_hex {
            return Self::new(&hex::decode(h.trim()).map_err(|_| CryptoError::BadKey)?);
        }
        match fs::read_to_string(path) {
            Ok(s) => Self::new(&hex::decode(s.trim()).map_err(|_| CryptoError::BadKey)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let key = random_bytes::<KEY_LEN>()?;
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                write_private(path, hex::encode(key).as_bytes())?;
                Self::new(&key)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn aad(purpose: Purpose, context: &str) -> Vec<u8> {
        let mut aad = Vec::with_capacity(purpose.tag().len() + 1 + context.len());
        aad.extend_from_slice(purpose.tag());
        aad.push(0);
        aad.extend_from_slice(context.as_bytes());
        aad
    }

    /// Returns `nonce || ciphertext`.
    pub fn encrypt(
        &self,
        purpose: Purpose,
        context: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let nonce_bytes = random_bytes::<NONCE_LEN>()?;
        let nonce = XNonce::from(nonce_bytes);
        let aad = Self::aad(purpose, context);
        let ct = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn decrypt(
        &self,
        purpose: Purpose,
        context: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let (nonce, ct) = data
            .split_at_checked(NONCE_LEN)
            .ok_or(CryptoError::Decrypt)?;
        let nonce = XNonce::try_from(nonce).map_err(|_| CryptoError::Decrypt)?;
        let aad = Self::aad(purpose, context);
        self.cipher
            .decrypt(&nonce, Payload { msg: ct, aad: &aad })
            .map_err(|_| CryptoError::Decrypt)
    }

    pub fn decrypt_string(
        &self,
        purpose: Purpose,
        context: &str,
        data: &[u8],
    ) -> Result<String, CryptoError> {
        String::from_utf8(self.decrypt(purpose, context, data)?).map_err(|_| CryptoError::Decrypt)
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(data)
}

/// Create or truncate a file only its owner can read (0600 on Unix), for
/// backups and other files holding secrets.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        o.mode(0o600);
        let f = o.open(path)?;
        // `mode` only applies to a new file.
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(f)
    }
    #[cfg(not(unix))]
    o.open(path)
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|_| CryptoError::Random)?;
    Ok(buf)
}

/// A new API key and the SHA-256 it is stored by: 32 random bytes, hex.
pub fn new_api_key() -> Result<(String, [u8; 32]), CryptoError> {
    let key = hex::encode(random_bytes::<32>()?);
    let hash = sha256(key.as_bytes());
    Ok((key, hash))
}

pub fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Subsonic token check: `t == md5(password + salt)`, compared in constant time.
pub fn verify_subsonic_token(password: &str, salt: &str, token: &str) -> bool {
    let mut input = Vec::with_capacity(password.len() + salt.len());
    input.extend_from_slice(password.as_bytes());
    input.extend_from_slice(salt.as_bytes());
    ct_eq(
        md5_hex(&input).as_bytes(),
        token.to_ascii_lowercase().as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_aad_binding() {
        let sb = SecretBox::new(&[7u8; 32]).unwrap();
        let ct = sb.encrypt(Purpose::Password, "alice", b"sesame").unwrap();
        assert_eq!(
            sb.decrypt_string(Purpose::Password, "alice", &ct).unwrap(),
            "sesame"
        );
        assert!(sb.decrypt(Purpose::Password, "bob", &ct).is_err());
        assert!(sb.decrypt(Purpose::BackendToken, "alice", &ct).is_err());
        let other = SecretBox::new(&[8u8; 32]).unwrap();
        assert!(other.decrypt(Purpose::Password, "alice", &ct).is_err());
    }

    #[test]
    fn subsonic_token() {
        // Example from the Subsonic API docs.
        assert!(verify_subsonic_token(
            "sesame",
            "c19b2d",
            "26719a1196d2a940705a59634eb18eab"
        ));
        assert!(!verify_subsonic_token(
            "sesame",
            "c19b2e",
            "26719a1196d2a940705a59634eb18eab"
        ));
    }

    #[test]
    fn key_file_is_created_then_reused() {
        let dir = std::env::temp_dir().join(format!(
            "rsub-key-{}",
            hex::encode(random_bytes::<6>().unwrap())
        ));
        let path = dir.join("secret.key");
        let a = SecretBox::load_or_create(None, &path).unwrap();
        let ct = a.encrypt(Purpose::KeyCheck, "", b"x").unwrap();
        let b = SecretBox::load_or_create(None, &path).unwrap();
        assert_eq!(b.decrypt(Purpose::KeyCheck, "", &ct).unwrap(), b"x");
        fs::remove_dir_all(dir).unwrap();
    }
}
