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

/// The stable, registerable install dir for Maxima's native binaries on macOS:
/// `~/Library/Application Support/Maxima/bin`. Every caller that needs to find
/// or spawn `maxima-server` — launchd, game-spawned bootstrap, and every
/// frontend — agrees on this path. It's stable across app moves/updates and
/// app-translocation (a quarantined `.app` runs from a randomized read-only
/// path, so a path *inside the bundle* would be unstable). See
/// docs/MACOS_BUNDLING.md. `None` on non-macOS.
pub fn app_support_bin_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join("Library/Application Support/Maxima/bin"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Locate the `maxima-server` binary: next to the current executable
/// (installer / cargo layout), then the stable App Support copy (macOS), then
/// `PATH` as a last resort.
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
    if let Some(dir) = app_support_bin_dir() {
        let p = dir.join(NAME);
        if p.is_file() {
            return Some(p);
        }
    }
    // Fall back to PATH.
    Some(std::path::PathBuf::from(NAME))
}

/// Sync the native binaries (and the bootstrap `.app`) sitting next to the
/// current executable into the stable App Support dir, so that after the very
/// first run from *any* layout (a `.app`, `target/release`, an installer temp
/// dir) launchd and game-spawned bootstrap can find `maxima-server` at the
/// well-known path. Cheap + idempotent (copies only when the source is newer);
/// best-effort (a failure just means the on-demand-spawn fallback is used).
/// macOS only — Windows/Linux register the server via their own installers.
#[cfg(target_os = "macos")]
pub fn ensure_app_support_install() {
    let Some(dest_dir) = app_support_bin_dir() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(src_dir) = exe.parent() else {
        return;
    };
    // Running from the canonical copy already — nothing to sync.
    if src_dir == dest_dir {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        log::warn!("app-support install: mkdir {} failed: {}", dest_dir.display(), e);
        return;
    }
    for name in ["maxima-server", "maxima-cli", "maxima-bootstrap"] {
        let src = src_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let dest = dest_dir.join(name);
        if let Err(e) = copy_if_newer(&src, &dest) {
            log::warn!("app-support install: copy {} failed: {}", name, e);
        }
    }
    // The `link2ea://` handler bundle (register-protocols registers it).
    let src_bundle = src_dir.join("bundle/osx/MaximaBootstrap.app");
    if src_bundle.is_dir() {
        let dest_parent = dest_dir.join("bundle/osx");
        let _ = std::fs::create_dir_all(&dest_parent);
        // Directory tree — std::fs::copy is file-only, so shell out.
        let _ = std::process::Command::new("/bin/cp")
            .arg("-R")
            .arg(&src_bundle)
            .arg(&dest_parent)
            .status();
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_app_support_install() {}

/// Copy `src` → `dest` when `dest` is missing or older than `src` (by mtime).
/// `std::fs::copy` carries the executable bit on unix, so the copied binary
/// stays runnable.
#[cfg(target_os = "macos")]
fn copy_if_newer(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    let need = match (std::fs::metadata(src), std::fs::metadata(dest)) {
        (Ok(s), Ok(d)) => match (s.modified(), d.modified()) {
            (Ok(sm), Ok(dm)) => sm > dm,
            _ => true,
        },
        (Ok(_), Err(_)) => true, // dest missing
        _ => false,              // src missing (caller already checked is_file)
    };
    if need {
        std::fs::copy(src, dest)?;
    }
    Ok(())
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
        // New session → the server is independent of this frontend's lifecycle
        // (a GUI/TUI that quits, or the launchd app-job it belongs to). A plain
        // process group isn't enough on macOS: children stay in the app's
        // launchd job and get reaped on quit. SETSID fully detaches.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }

    let _ = cmd.spawn();
}
