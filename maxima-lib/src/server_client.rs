//! Everything a frontend or the host does to **manage the `maxima-server`
//! process** — find it, spawn it on demand, sync its binaries to a stable
//! path, and register / unregister it with the OS (the boot policy). One module
//! so the whole server lifecycle lives in one place (and so "service" naming
//! doesn't collide with `util::service` — the Windows KYBER OS-service — or the
//! `maxima-service` crate).
//!
//! The discovery/spawn half is std-only (no async, no `maxima-proto`) so any
//! frontend can call it from any startup path. A frontend's own `start_lsx`
//! probe defers to the server when the port is already bound, so they coexist.
//!
//! Boot policy (persisted in `config.json`, read by every frontend incl. the
//! Swift app which reads the JSON directly):
//!   - **Auto**     — the OS starts the server at login (autostart installed).
//!   - **OnDemand** — no autostart; a frontend or a game launch spawns the
//!                    (detached) server when it opens. The default.
//!   - **Manual**   — no autostart, no auto-spawn; the user starts it explicitly.
//!
//! OS registration, per platform: macOS launchd LaunchAgent (a **classic**
//! agent, not `SMAppService` — that needs a real signing identity and ad-hoc
//! doesn't work), Windows `HKCU\…\Run`, Linux systemd `--user` unit. See
//! docs/MACOS_BUNDLING.md.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Boxed-error result — maxima-lib keeps typed errors, but service management
/// glues together fs / process / registry calls whose errors we only ever log,
/// so a boxed error keeps this section dependency-light.
type BoxResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// launchd label / Windows Run value / systemd unit stem.
pub const SERVICE_LABEL: &str = "com.armchairdevelopers.maxima.server";

// =========================================================================
// Discovery + spawn (std-only; callable from any frontend)
// =========================================================================

/// Control port (matches `maxima_proto::DEFAULT_PORT`); honors `MAXIMA_SERVER_PORT`.
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
pub fn app_support_bin_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/Maxima/bin"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Locate the `maxima-server` binary: next to the current executable
/// (installer / cargo layout), then the stable App Support copy (macOS), then
/// `PATH` as a last resort (which is why this always returns a path).
pub fn locate_server() -> PathBuf {
    #[cfg(windows)]
    const NAME: &str = "maxima-server.exe";
    #[cfg(not(windows))]
    const NAME: &str = "maxima-server";

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(NAME);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    if let Some(dir) = app_support_bin_dir() {
        let p = dir.join(NAME);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from(NAME)
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
/// booting). Best-effort. Respects the boot policy — under `Manual` the user
/// starts the server themselves, so a frontend must not auto-spawn it.
pub fn ensure_running() {
    let port = server_port();
    if is_running(port) {
        return;
    }
    if !boot_policy().auto_spawn_on_open() {
        return;
    }

    let mut cmd = std::process::Command::new(locate_server());
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

// =========================================================================
// Boot policy (persisted config)
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootPolicy {
    Auto,
    OnDemand,
    Manual,
}

impl BootPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on-demand" | "ondemand" | "demand" => Some(Self::OnDemand),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OnDemand => "on-demand",
            Self::Manual => "manual",
        }
    }

    /// Whether a frontend (or the bootstrap on a game launch) should spawn the
    /// detached server if it isn't already running. True for everything except
    /// `Manual`. Under `Auto`, launchd already runs it — but spawning as a
    /// fallback when the agent didn't come up is harmless (`ensure_running`
    /// no-ops if the port already answers).
    pub fn auto_spawn_on_open(&self) -> bool {
        !matches!(self, Self::Manual)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    boot_policy: Option<BootPolicy>,
}

/// The Maxima config directory (holds `config.json`). Same location Swift/egui
/// read for the boot policy.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Maxima"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Maxima"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("maxima"))
    }
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.json"))
}

fn read_config() -> Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_config(cfg: &Config) -> BoxResult<()> {
    let path = config_path().ok_or("no config dir (HOME unset?)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// The effective boot policy. Defaults to `OnDemand` when unset — the safest
/// "just works when you open a frontend, nothing left running otherwise" mode.
pub fn boot_policy() -> BootPolicy {
    read_config().boot_policy.unwrap_or(BootPolicy::OnDemand)
}

fn set_boot_policy(policy: BootPolicy) -> BoxResult<()> {
    let mut cfg = read_config();
    cfg.boot_policy = Some(policy);
    write_config(&cfg)
}

// =========================================================================
// OS registration (install / uninstall / status)
// =========================================================================

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub policy: String,
    pub autostart_installed: bool,
    pub running: bool,
}

/// Install / update the service registration for `policy`, syncing the binaries
/// to the stable path first. `Auto` installs the OS autostart; `OnDemand` /
/// `Manual` remove it (frontends handle spawning per the policy).
pub fn install(policy: BootPolicy) -> BoxResult<()> {
    set_boot_policy(policy)?;
    ensure_app_support_install();

    match policy {
        BootPolicy::Auto => install_autostart()?,
        BootPolicy::OnDemand | BootPolicy::Manual => {
            // No OS autostart in these modes; make sure a stale one is gone.
            let _ = remove_autostart();
        }
    }
    log::info!("Maxima server boot policy set to '{}'", policy.as_str());
    Ok(())
}

/// Remove **everything** — the autostart registration, the protocol handler
/// registration, the installed binaries, and the config — so no trace is left
/// that could interfere with the official EA launcher. With `purge`, also
/// removes cached auth tokens and logs (`maxima_dir`). Game bottles are left
/// alone (they're large and separate); remove them from CrossOver manually.
pub fn uninstall(purge: bool) -> BoxResult<()> {
    // Stop a running server first (best-effort).
    stop_running_server();
    let _ = remove_autostart();
    unregister_protocols();
    remove_installed_binaries();
    if let Some(path) = config_path() {
        let _ = std::fs::remove_file(path);
    }
    if purge {
        if let Ok(dir) = crate::util::native::maxima_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    log::info!("Maxima server uninstalled{}", if purge { " (purged tokens + logs)" } else { "" });
    Ok(())
}

pub fn status() -> ServiceStatus {
    ServiceStatus {
        policy: boot_policy().as_str().to_string(),
        autostart_installed: autostart_installed(),
        running: is_running(server_port()),
    }
}

fn stop_running_server() {
    use std::io::Write;
    let port = server_port();
    if is_running(port) {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"{\"id\":1,\"cmd\":\"shutdown\"}\n");
            let _ = s.flush();
        }
    }
}

fn remove_installed_binaries() {
    // The App Support bin dir holds the synced binaries + bootstrap app.
    if let Some(bin) = app_support_bin_dir() {
        let _ = std::fs::remove_dir_all(&bin);
    }
}

// --- macOS — launchd LaunchAgent ---

#[cfg(target_os = "macos")]
fn agent_plist_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", SERVICE_LABEL))
    })
}

#[cfg(target_os = "macos")]
fn install_autostart() -> BoxResult<()> {
    use std::process::Command;

    let plist = agent_plist_path().ok_or("no HOME")?;
    let server = app_support_bin_dir()
        .map(|d| d.join("maxima-server"))
        .ok_or("no HOME")?;
    if !server.is_file() {
        return Err(format!(
            "maxima-server not found at {} — install the binaries first",
            server.display()
        )
        .into());
    }
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{server}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>/tmp/maxima-server.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/maxima-server.err.log</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        server = server.display(),
    );
    std::fs::write(&plist, contents)?;

    let uid = unsafe { libc::getuid() };
    // Reload cleanly: bootout an existing instance (ignore failure), then
    // bootstrap the new one.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid, SERVICE_LABEL)])
        .status();
    let status = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{}", uid)])
        .arg(&plist)
        .status()?;
    if !status.success() {
        // Fall back to the legacy verb on older systems.
        let _ = Command::new("launchctl").args(["load", "-w"]).arg(&plist).status();
    }
    log::info!("Installed launchd agent {}", plist.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_autostart() -> BoxResult<()> {
    use std::process::Command;
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid, SERVICE_LABEL)])
        .status();
    if let Some(plist) = agent_plist_path() {
        let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&plist).status();
        let _ = std::fs::remove_file(&plist);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn autostart_installed() -> bool {
    agent_plist_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn unregister_protocols() {
    // Unregister the MaximaBootstrap.app claim (qrc:// / link2ea:// / origin2://)
    // from LaunchServices so we stop intercepting those schemes.
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    for base in [
        app_support_bin_dir(),
        std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())),
    ]
    .into_iter()
    .flatten()
    {
        let app = base.join("bundle/osx/MaximaBootstrap.app");
        if app.is_dir() {
            let _ = std::process::Command::new(LSREGISTER).arg("-u").arg(&app).status();
        }
    }
}

// --- Windows — HKCU\…\Run ---

#[cfg(windows)]
fn install_autostart() -> BoxResult<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let server = app_support_bin_dir()
        .map(|d| d.join("maxima-server.exe"))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.join("maxima-server.exe")))
        })
        .ok_or("could not locate maxima-server.exe")?;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")?;
    run.set_value("MaximaServer", &format!("\"{}\"", server.display()))?;
    log::info!("Registered HKCU Run\\MaximaServer");
    Ok(())
}

#[cfg(windows)]
fn remove_autostart() -> BoxResult<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(run) = hkcu.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        winreg::enums::KEY_ALL_ACCESS,
    ) {
        let _ = run.delete_value("MaximaServer");
    }
    Ok(())
}

#[cfg(windows)]
fn autostart_installed() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .and_then(|run| run.get_value::<String, _>("MaximaServer"))
        .is_ok()
}

#[cfg(windows)]
fn unregister_protocols() {
    // The NSIS installer owns HKCR protocol handlers on Windows; nothing to do
    // here (uninstalling Maxima runs its uninstaller). Left as a no-op so the
    // cross-platform uninstall path is uniform.
}

// --- Linux — systemd --user unit ---

#[cfg(all(unix, not(target_os = "macos")))]
fn systemd_unit_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|c| c.join("systemd/user/maxima-server.service"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_autostart() -> BoxResult<()> {
    use std::process::Command;
    let unit = systemd_unit_path().ok_or("no HOME")?;
    let server = app_support_bin_dir()
        .map(|d| d.join("maxima-server"))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.join("maxima-server")))
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("maxima-server"));
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = format!(
        "[Unit]\nDescription=Maxima server\n\n[Service]\nExecStart={}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        server.display()
    );
    std::fs::write(&unit, contents)?;
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "--now", "maxima-server.service"])
        .status();
    log::info!("Installed systemd user unit {}", unit.display());
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn remove_autostart() -> BoxResult<()> {
    use std::process::Command;
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "maxima-server.service"])
        .status();
    if let Some(unit) = systemd_unit_path() {
        let _ = std::fs::remove_file(&unit);
    }
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn autostart_installed() -> bool {
    systemd_unit_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unregister_protocols() {
    // Remove the maxima-*.desktop scheme handlers written by set_up_registry.
    if let Some(apps) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("applications"))
    {
        for proto in ["qrc", "link2ea", "origin2"] {
            let _ = std::fs::remove_file(apps.join(format!("maxima-{}.desktop", proto)));
        }
    }
}
