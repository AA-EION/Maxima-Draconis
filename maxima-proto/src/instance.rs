//! How a client finds *its own* Maxima server.
//!
//! Every Maxima installation context — the macOS/Linux/Windows user, and each
//! Wine prefix with Maxima installed inside it — has its own data directory.
//! Wine prefixes share the host's loopback interface, so a well-known port
//! would let a client in one prefix (or on the host) reach the server of
//! another. Instead, a running server publishes `instance.json` in its data
//! directory: the ports it actually bound and a random per-run token. A client
//! reads the file from its own data directory and proves the token in its
//! first message, so it can only ever talk to the server of its own context.
//!
//! The server holds an exclusive lock on `instance.lock` for its whole
//! lifetime. That keeps a second server of the same context from starting,
//! and lets a client tell a live server from a file left behind by a crash.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const INSTANCE_FILE: &str = "instance.json";
pub const LOCK_FILE: &str = "instance.lock";
const REALM_FILE: &str = "realm.json";

/// Version of the control protocol spoken after `hello`.
pub const PROTO_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceState {
    /// Listening, but the EA login hasn't finished; session requests are
    /// refused with `login-pending`.
    AwaitingLogin,
    Ready,
}

/// Contents of `instance.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InstanceInfo {
    pub schema: u32,
    /// Stable id of this installation context (persisted in `realm.json`).
    pub realm: String,
    pub pid: u32,
    pub version: String,
    pub state: InstanceState,
    /// Secret every client must present; regenerated on each server start.
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsx_port: Option<u16>,
}

/// What a client finds in a data directory.
#[derive(Debug, Clone, PartialEq)]
pub enum Discovery {
    Running(InstanceInfo),
    /// A server holds the lock but hasn't published its ports yet.
    Starting,
    Stopped,
}

/// Look for a live server in `data_dir`.
pub fn discover(data_dir: &Path) -> Discovery {
    if !lock_held(data_dir) {
        return Discovery::Stopped;
    }
    match read(data_dir) {
        Some(info) => Discovery::Running(info),
        None => Discovery::Starting,
    }
}

fn read(data_dir: &Path) -> Option<InstanceInfo> {
    let data = fs::read(data_dir.join(INSTANCE_FILE)).ok()?;
    serde_json::from_slice(&data).ok()
}

fn lock_held(data_dir: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(data_dir.join(LOCK_FILE)) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

/// Held by a running server: owns the lock and the published `instance.json`,
/// which is removed again when the guard drops.
pub struct InstanceGuard {
    dir: PathBuf,
    info: InstanceInfo,
    _lock: File,
}

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("another Maxima server is already running for {0}")]
    AlreadyRunning(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl InstanceGuard {
    /// Take the instance lock for `data_dir`, before binding any listener.
    pub fn acquire(data_dir: &Path, version: &str) -> Result<Self, GuardError> {
        fs::create_dir_all(data_dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(data_dir.join(LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(GuardError::AlreadyRunning(data_dir.to_path_buf()))
            }
            Err(TryLockError::Error(err)) => return Err(err.into()),
        }

        let info = InstanceInfo {
            schema: 1,
            realm: load_or_create_realm(data_dir)?,
            pid: std::process::id(),
            version: version.to_owned(),
            state: InstanceState::AwaitingLogin,
            token: random_hex(32)?,
            control_port: None,
            authorize_port: None,
            lsx_port: None,
        };
        // A file left by a crashed server is stale by definition now.
        let _ = fs::remove_file(data_dir.join(INSTANCE_FILE));
        Ok(Self { dir: data_dir.to_path_buf(), info, _lock: lock })
    }

    pub fn info(&self) -> &InstanceInfo {
        &self.info
    }

    /// Update the published state (atomic replace, owner-only on unix).
    pub fn publish(&mut self, update: impl FnOnce(&mut InstanceInfo)) -> io::Result<()> {
        update(&mut self.info);
        let tmp = self.dir.join(format!("{INSTANCE_FILE}.tmp"));
        {
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(&self.info)?)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, self.dir.join(INSTANCE_FILE))
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(INSTANCE_FILE));
    }
}

#[derive(Serialize, Deserialize)]
struct Realm {
    id: String,
}

fn load_or_create_realm(data_dir: &Path) -> io::Result<String> {
    let path = data_dir.join(REALM_FILE);
    if let Some(realm) = fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Realm>(&data).ok())
    {
        return Ok(realm.id);
    }
    let id = format!("mx-{}", random_hex(16)?);
    fs::write(&path, serde_json::to_vec(&Realm { id: id.clone() })?)?;
    Ok(id)
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|err| io::Error::other(err.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time comparison for the session token.
pub fn token_matches(expected: &str, presented: &str) -> bool {
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("maxima-instance-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn guard_is_exclusive_and_publishes() {
        let dir = temp_dir("exclusive");
        assert_eq!(discover(&dir), Discovery::Stopped);

        let mut guard = InstanceGuard::acquire(&dir, "1.0").unwrap();
        assert_eq!(discover(&dir), Discovery::Starting);
        assert!(matches!(
            InstanceGuard::acquire(&dir, "1.0"),
            Err(GuardError::AlreadyRunning(_))
        ));

        guard.publish(|info| info.control_port = Some(4242)).unwrap();
        match discover(&dir) {
            Discovery::Running(info) => {
                assert_eq!(info.control_port, Some(4242));
                assert_eq!(info.token.len(), 64);
            }
            other => panic!("expected running, got {other:?}"),
        }

        drop(guard);
        assert_eq!(discover(&dir), Discovery::Stopped);
        assert!(!dir.join(INSTANCE_FILE).exists());
    }

    #[test]
    fn stale_file_without_lock_is_stopped() {
        let dir = temp_dir("stale");
        fs::write(dir.join(INSTANCE_FILE), b"{}").unwrap();
        assert_eq!(discover(&dir), Discovery::Stopped);
    }

    #[test]
    fn realm_survives_restarts_and_tokens_do_not() {
        let dir = temp_dir("realm");
        let first = InstanceGuard::acquire(&dir, "1.0").unwrap();
        let (realm, token) = (first.info().realm.clone(), first.info().token.clone());
        drop(first);
        let second = InstanceGuard::acquire(&dir, "1.0").unwrap();
        assert_eq!(second.info().realm, realm);
        assert_ne!(second.info().token, token);
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abcd", "abcd"));
        assert!(!token_matches("abcd", "abce"));
        assert!(!token_matches("abcd", "abc"));
    }
}
