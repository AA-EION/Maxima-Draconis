//! Tiny client-side helper shared by every frontend (egui UI, TUI, CLI) so
//! that launching any of them brings the Maxima server up when it isn't
//! already running (and wasn't started at logon). The server owns LSX +
//! `/authorize` + RTM; a frontend's own `start_lsx` probe defers to it when
//! the port is already bound, so they coexist.
//!
//! This is intentionally minimal (std only, no async) so it can be called
//! from any startup path regardless of the frontend's runtime.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Control port (matches `server::DEFAULT_PORT`); honors `MAXIMA_SERVER_PORT`.
pub fn server_port() -> u16 {
    std::env::var("MAXIMA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(13220)
}

/// True if a server answers on the control port.
pub fn is_running(port: u16) -> bool {
    let addr = match ("127.0.0.1", port).to_socket_addrs() {
        Ok(mut a) => match a.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Locate the `maxima-server` binary: next to the current executable
/// (installer / cargo layout) or on `PATH` as a last resort.
fn locate_server() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    const NAME: &str = "maxima-server.exe";
    #[cfg(not(windows))]
    const NAME: &str = "maxima-server";

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(NAME);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    // Fall back to PATH.
    Some(std::path::PathBuf::from(NAME))
}

/// Ensure the Maxima server is running: if the control port doesn't answer,
/// spawn `maxima-server` detached so it outlives this frontend. Returns
/// immediately after spawning (does not wait for the server to finish
/// booting). Best-effort — errors are swallowed since the frontend can still
/// run its own in-process session if the spawn fails.
pub fn ensure_running() {
    let port = server_port();
    if is_running(port) {
        return;
    }
    let Some(server) = locate_server() else { return };

    let mut cmd = std::process::Command::new(server);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }

    let _ = cmd.spawn();
}
