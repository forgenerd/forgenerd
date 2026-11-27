use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Signature panel port.
pub const DEFAULT_PORT: u16 = 4367;
const DEFAULT_DATA_DIR: &str = "/var/lib/forgenerd";
const MASTER_KEY_FILE: &str = "master.key";

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let data_dir = std::env::var_os("FORGENERD_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
        let listen = match std::env::var("FORGENERD_LISTEN") {
            Ok(v) => v
                .parse()
                .with_context(|| format!("invalid FORGENERD_LISTEN: {v}"))?,
            Err(_) => SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
        };
        Ok(Self { data_dir, listen })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("forgenerd.db")
    }

    /// Create the data dir and make its path absolute (Docker bind mounts require it).
    pub fn ensure_data_dir(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data dir {}", self.data_dir.display()))?;
        self.data_dir = std::fs::canonicalize(&self.data_dir)
            .with_context(|| format!("resolving data dir {}", self.data_dir.display()))?;
        Ok(())
    }
}

/// Resolve the 32-byte master key: `FORGENERD_MASTER_KEY` (base64), else
/// `<data_dir>/master.key`, else generate a new one with 0600 permissions.
pub fn load_or_create_master_key(data_dir: &Path) -> Result<[u8; 32]> {
    if let Ok(encoded) = std::env::var("FORGENERD_MASTER_KEY") {
        return decode_key(encoded.trim()).context("invalid FORGENERD_MASTER_KEY");
    }

    let path = data_dir.join(MASTER_KEY_FILE);
    if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        return decode_key(contents.trim()).with_context(|| format!("invalid {}", path.display()));
    }

    let mut key = [0u8; 32];
    rand::fill(&mut key);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(STANDARD.encode(key).as_bytes())?;
    file.sync_all()?;
    tracing::warn!(
        path = %path.display(),
        "generated new master key: BACK IT UP separately from the database; losing it loses every stored secret"
    );
    Ok(key)
}

fn decode_key(encoded: &str) -> Result<[u8; 32]> {
    let bytes = STANDARD.decode(encoded).context("not valid base64")?;
    match <[u8; 32]>::try_from(bytes.as_slice()) {
        Ok(key) => Ok(key),
        Err(_) => bail!("master key must be 32 bytes, got {}", bytes.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn generates_then_reloads_key_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_master_key(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(MASTER_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let second = load_or_create_master_key(dir.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(decode_key(&STANDARD.encode([0u8; 16])).is_err());
    }
}
