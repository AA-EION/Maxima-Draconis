//! Cross-platform management of the Maxima background **service** — the
//! `maxima-server` process every frontend and game talks to. Install / register
//! it with the OS, uninstall it cleanly (leaving no trace so it can't interfere
//! with the official EA launcher), and expose the **boot policy** that frontends
//! read to decide whether to auto-spawn it.
//!
//! Placement / registration, per OS:
//!   - **macOS** — a classic launchd LaunchAgent in `~/Library/LaunchAgents`.
//!     We deliberately do *not* use `SMAppService`: it requires a real
//!     Apple-issued signing identity, and ad-hoc ("Sign to Run Locally")
//!     signing doesn't work with it. A plain LaunchAgent has no signing
//!     requirement. Its `ProgramArguments` points at the binary in the stable
//!     App Support dir (see [`crate::server_client::app_support_bin_dir`]) —
//!     not inside the `.app`, which is unstable under app-translocation.
//!   - **Windows** — an `HKCU\…\Run` value.
//!   - **Linux** — a systemd `--user` unit.
//!
//! Boot policy (persisted in the Maxima config, read by every frontend via
//! [`boot_policy`]):
//!   - **Auto**     — the OS starts the server at login (agent installed).
//!   - **OnDemand** — no OS autostart; a frontend or a game launch spawns the
//!                    (detached) server when it opens.
//!   - **Manual**   — no autostart, no auto-spawn; the user starts it explicitly.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Boxed-error result — maxima-lib keeps typed errors, but service management
/// glues together fs / process / registry calls whose errors we only ever log,
/// so a boxed error keeps this module dependency-light.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// launchd label / Windows Run value / systemd unit stem.
pub const SERVICE_LABEL: &str = "com.armchairdevelopers.maxima.server";

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
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/Maxima"))
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

fn write_config(cfg: &Config) -> Result<()> {
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

fn set_boot_policy(policy: BootPolicy) -> Result<()> {
    let mut cfg = read_config();
    cfg.boot_policy = Some(policy);
    write_config(&cfg)
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub policy: String,
    pub autostart_installed: bool,
    pub running: bool,
}

/// Install / update the service registration for `policy`, syncing the binaries
/// to the stable path first. `Auto` installs the OS autostart; `OnDemand` /
/// `Manual` remove it (frontends handle spawning per the policy).
pub fn install(policy: BootPolicy) -> Result<()> {
    set_boot_policy(policy)?;
    crate::server_client::ensure_app_support_install();

    match policy {
        BootPolicy::Auto => install_autostart()?,
        BootPolicy::OnDemand | BootPolicy::Manual => {
            // No OS autostart in these modes; make sure a stale one is gone.
            let _ = remove_autostart();
        }
    }
    log::info!("Maxima service boot policy set to '{}'", policy.as_str());
    Ok(())
}

/// Remove **everything** — the autostart registration, the protocol handler
/// registration, the installed binaries, and the config — so no trace is left
/// that could interfere with the official EA launcher. With `purge`, also
/// removes cached auth tokens and logs (`maxima_dir`). Game bottles are left
/// alone (they're large and separate); remove them from CrossOver manually.
pub fn uninstall(purge: bool) -> Result<()> {
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
    log::info!("Maxima service uninstalled{}", if purge { " (purged tokens + logs)" } else { "" });
    Ok(())
}

pub fn status() -> ServiceStatus {
    ServiceStatus {
        policy: boot_policy().as_str().to_string(),
        autostart_installed: autostart_installed(),
        running: crate::server_client::is_running(crate::server_client::server_port()),
    }
}

fn stop_running_server() {
    use std::io::Write;
    let port = crate::server_client::server_port();
    if crate::server_client::is_running(port) {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"{\"id\":1,\"cmd\":\"shutdown\"}\n");
            let _ = s.flush();
        }
    }
}

fn remove_installed_binaries() {
    // The App Support bin dir holds the synced binaries + bootstrap app.
    if let Some(bin) = crate::server_client::app_support_bin_dir() {
        let _ = std::fs::remove_dir_all(&bin);
    }
}

// =========================================================================
// macOS — launchd LaunchAgent
// =========================================================================

#[cfg(target_os = "macos")]
fn agent_plist_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", SERVICE_LABEL))
    })
}

#[cfg(target_os = "macos")]
fn server_binary_path() -> Option<PathBuf> {
    crate::server_client::app_support_bin_dir().map(|d| d.join("maxima-server"))
}

#[cfg(target_os = "macos")]
fn install_autostart() -> Result<()> {
    use std::process::Command;

    let plist = agent_plist_path().ok_or("no HOME")?;
    let server = server_binary_path().ok_or("no HOME")?;
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
fn remove_autostart() -> Result<()> {
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
        crate::server_client::app_support_bin_dir(),
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

// =========================================================================
// Windows — HKCU\…\Run
// =========================================================================

#[cfg(windows)]
fn install_autostart() -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let server = crate::server_client::app_support_bin_dir()
        .map(|d| d.join("maxima-server.exe"))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.join("maxima-server.exe")))
        })
        .ok_or("could not locate maxima-server.exe")?;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) =
        hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")?;
    run.set_value("MaximaServer", &format!("\"{}\"", server.display()))?;
    log::info!("Registered HKCU Run\\MaximaServer");
    Ok(())
}

#[cfg(windows)]
fn remove_autostart() -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(run) =
        hkcu.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", winreg::enums::KEY_ALL_ACCESS)
    {
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

// =========================================================================
// Linux — systemd --user unit
// =========================================================================

#[cfg(all(unix, not(target_os = "macos")))]
fn systemd_unit_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|c| c.join("systemd/user/maxima-server.service"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_autostart() -> Result<()> {
    use std::process::Command;
    let unit = systemd_unit_path().ok_or("no HOME")?;
    let server = crate::server_client::app_support_bin_dir()
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
fn remove_autostart() -> Result<()> {
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
