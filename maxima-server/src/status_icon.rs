//! The server's status-bar presence — one entry point, one idiom per OS.
//!
//! The rule the product sets: the **server** owns the icon, so you can stop it
//! or open the UI even when no window is up. Each OS uses its native idiom:
//!
//!   * **Windows** — native tray in-process (`Shell_NotifyIcon`, [`crate::tray`]).
//!   * **macOS**   — spawn the menu-bar host (`Maxima.app --menubar`), which
//!                   draws the SwiftUI `MenuBarExtra` and connects back as a
//!                   client (status items require a GUI app; the server is
//!                   headless).
//!   * **Linux**   — SNI tray behind the `linux-tray` cargo feature; headless
//!                   by default.
//!
//! Menu on every platform: **Open Maxima** · **Stop Server**. See
//! docs/MACOS_BUNDLING.md.

/// Bring up the server's status-bar icon. Never blocks; best-effort — a
/// failure just means no icon (the server still runs and is CLI-drivable).
pub fn spawn(port: u16) {
    #[cfg(windows)]
    crate::tray::spawn_tray(port);

    #[cfg(target_os = "macos")]
    {
        let _ = port;
        macos::spawn_menubar_host();
    }

    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    crate::linux_tray::spawn_sni(port);

    #[cfg(all(target_os = "linux", not(feature = "linux-tray")))]
    {
        let _ = port;
        log::info!(
            "Linux: running headless (build maxima-server with --features linux-tray for an \
             SNI status icon). Drive it with `maxima-cli server-status` / `server-stop`."
        );
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;
    use std::process::Command;

    /// Status items need a GUI process; the server is headless. Spawn
    /// `Maxima.app` in menu-bar mode — it draws the `MenuBarExtra` and connects
    /// back to us as a client. `open` coalesces to the single running instance,
    /// so this is safe even when the app already launched us. If Maxima.app
    /// isn't installed we stay headless.
    pub fn spawn_menubar_host() {
        // Preferred: by bundle id (works once LaunchServices knows the app).
        let by_id = Command::new("/usr/bin/open")
            .args([
                "-g", // don't steal focus
                "-b",
                "com.armchairdevelopers.maxima.native",
                "--args",
                "--menubar",
            ])
            .status();
        if matches!(by_id, Ok(s) if s.success()) {
            log::info!("Spawned Maxima.app menu-bar host for the status icon.");
            return;
        }

        // Fallback: resolve the bundle path (bundle id not registered yet).
        if let Some(app) = resolve_app_bundle() {
            let _ = Command::new("/usr/bin/open")
                .arg("-g")
                .arg(&app)
                .args(["--args", "--menubar"])
                .status();
            log::info!("Spawned {} for the status icon.", app.display());
        } else {
            log::info!(
                "Maxima.app not found — running without a menu-bar icon (use \
                 `maxima-cli server-status` / `server-stop`)."
            );
        }
    }

    /// Best-effort resolve Maxima.app: three parents up when we run from inside
    /// the bundle (…/Maxima.app/Contents/Resources/maxima-server), else the
    /// standard install spots.
    fn resolve_app_bundle() -> Option<PathBuf> {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(app) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
                if app.extension().map(|e| e == "app").unwrap_or(false) && app.is_dir() {
                    return Some(app.to_path_buf());
                }
            }
        }
        for p in ["/Applications/Maxima.app"] {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                return Some(pb);
            }
        }
        None
    }
}
