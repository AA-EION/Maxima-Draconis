use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs::{create_dir_all, remove_dir_all, remove_file, File},
    io::Read,
    path::PathBuf,
    process::{ExitStatus, Stdio},
};

use flate2::read::GzDecoder;
use lazy_static::lazy_static;
use log::{info, warn};
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};
use xz2::read::XzDecoder;

use crate::util::{
    github::{fetch_github_release, fetch_github_releases, github_download_asset, GithubRelease},
    native::{maxima_dir, DownloadError, NativeError, SafeParent, SafeStr, WineError},
    registry::RegistryError,
};

lazy_static! {
    static ref PROTON_PATTERN: Regex = Regex::new(r"GE-Proton\d+-\d+\.tar\.gz").unwrap();
}

// A Proton verb to use
pub enum CommandType {
    // Set the prefix up and runs the command
    Run,
    // Waits for any hanging wineserver instances and runs the command
    WaitForExitAndRun,
    // Directly calls the command, doesn't setup the prefix (use with caution)
    RunInPrefix,
}

impl std::fmt::Display for CommandType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let display = match self {
            Self::RunInPrefix => "runinprefix",
            Self::Run => "run",
            Self::WaitForExitAndRun => "waitforexitandrun",
        };
        f.write_str(display)
    }
}

const VERSION_FILE: &str = "dependency-versions.toml";

#[derive(Deserialize, Default)]
pub(crate) struct LutrisRuntime {
    name: String,
    created_at: String,
    url: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Versions {
    proton: String,
    eac_runtime: String,
    umu: String,
}

/// Returns internal prtoton pfx path
pub fn wine_prefix_dir() -> Result<PathBuf, NativeError> {
    // Override to target an existing prefix — on macOS this is how a
    // CrossOver bottle is selected, e.g.
    // MAXIMA_WINE_PREFIX="$HOME/Library/Application Support/CrossOver/Bottles/Titanfall 2"
    if let Ok(prefix) = env::var("MAXIMA_WINE_PREFIX") {
        return Ok(PathBuf::from(prefix));
    }
    Ok(maxima_dir()?.join("wine/prefix"))
}

/// CrossOver's wine loader on macOS — used as the default wine command
/// when present and MAXIMA_WINE_COMMAND isn't set.
#[cfg(target_os = "macos")]
pub const CROSSOVER_WINE: &str =
    "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/wine";

/// CrossOver's launch helper. Games/installers are handed off through this
/// instead of running `wine` as our descendant: cxstart makes the launch
/// behave exactly like double-clicking the exe inside CrossOver's UI, with
/// CrossOver owning the process tree. Running wine directly works from a
/// shell but freezes the game's renderer (blank window right after LSX
/// GetAllGameInfo) when Maxima itself is a `.app`-launched GUI — the same
/// failure Draconis solved by delegating to cxstart. Env vars still
/// propagate into the Windows environment through cxstart (verified).
#[cfg(target_os = "macos")]
pub const CROSSOVER_CXSTART: &str =
    "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/cxstart";

/// posix_spawn with the exact attribute set Draconis's CleanSpawn uses for
/// its (working) game launches from a `.app`: `POSIX_SPAWN_CLOEXEC_DEFAULT`
/// + `POSIX_SPAWN_SETSID` + `responsibility_spawnattrs_setdisclaim`, with
/// /dev/null stdio. The disclaim must be applied at THIS hop: the game
/// inherits its "responsible process" from cxstart, and disclaiming only an
/// upstream headless process leaves the game attributed to something with
/// no GUI check-in — wine's display driver then starves waiting on
/// WindowServer events (frozen blank window, main thread parked in
/// mach_msg). Returns the child pid.
#[cfg(target_os = "macos")]
fn spawn_disclaimed(
    exe: &str,
    args: &[std::ffi::OsString],
) -> Result<libc::pid_t, NativeError> {
    use std::ffi::CString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    // Not exported by the libc crate for apple targets; value from <spawn.h>.
    const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;

    let c_exe = CString::new(exe).map_err(|_| NativeError::Stringify)?;
    let mut c_args: Vec<CString> = vec![c_exe.clone()];
    for arg in args {
        c_args.push(CString::new(arg.as_bytes()).map_err(|_| NativeError::Stringify)?);
    }
    let mut argv: Vec<*mut libc::c_char> =
        c_args.iter().map(|c| c.as_ptr() as *mut _).collect();
    argv.push(std::ptr::null_mut());

    let env_strings: Vec<CString> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let mut pair = key.into_vec();
            pair.push(b'=');
            pair.extend_from_slice(value.as_bytes());
            CString::new(pair).ok()
        })
        .collect();
    let mut envp: Vec<*mut libc::c_char> =
        env_strings.iter().map(|c| c.as_ptr() as *mut _).collect();
    envp.push(std::ptr::null_mut());

    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return Err(NativeError::Io(std::io::Error::last_os_error()));
        }
        libc::posix_spawnattr_setflags(
            &mut attr,
            (libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short) | POSIX_SPAWN_SETSID,
        );

        // Private but stable since 10.14; resolved dynamically so a future
        // macOS removing it degrades gracefully. Same call Draconis makes.
        let disclaim_sym = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_spawnattrs_setdisclaim".as_ptr(),
        );
        if !disclaim_sym.is_null() {
            let disclaim: extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int =
                std::mem::transmute(disclaim_sym);
            disclaim(&mut attr, 1);
        }

        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut actions);
        let devnull = c"/dev/null".as_ptr();
        libc::posix_spawn_file_actions_addopen(&mut actions, 0, devnull, libc::O_RDONLY, 0);
        libc::posix_spawn_file_actions_addopen(&mut actions, 1, devnull, libc::O_WRONLY, 0);
        libc::posix_spawn_file_actions_adddup2(&mut actions, 1, 2);

        let mut pid: libc::pid_t = 0;
        let rc = libc::posix_spawn(
            &mut pid,
            c_exe.as_ptr(),
            &actions,
            &attr,
            argv.as_ptr(),
            envp.as_ptr(),
        );
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);

        if rc != 0 {
            return Err(NativeError::Io(std::io::Error::from_raw_os_error(rc)));
        }
        Ok(pid)
    }
}

/// Run a Windows exe via cxstart. cxstart exits right after the handoff, so
/// wait-for-exit semantics are emulated by polling for the exe's process:
/// grace period for it to appear (CrossOver cold start), then wait until
/// it's gone.
#[cfg(target_os = "macos")]
async fn run_via_cxstart(
    prefix: &std::path::Path,
    exe: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
) -> Result<String, NativeError> {
    use sysinfo::{ProcessExt, System, SystemExt};

    let bottle = prefix
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    info!(
        "Launching {:?} via cxstart (bottle '{}', disclaimed spawn)",
        exe, bottle
    );

    let mut cx_args: Vec<std::ffi::OsString> =
        vec!["--bottle".into(), bottle.clone().into(), exe.clone()];
    cx_args.extend(args);

    let pid = spawn_disclaimed(CROSSOVER_CXSTART, &cx_args)?;

    // Reap cxstart itself (it exits once the handoff is done).
    let cx_status = tokio::task::spawn_blocking(move || {
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        status
    })
    .await
    .unwrap_or(0);
    if cx_status != 0 {
        warn!("cxstart exited with raw status {}", cx_status);
    }

    let needle = std::path::Path::new(&exe)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if needle.is_empty() {
        return Ok(String::new());
    }

    let running = |needle: &str| -> bool {
        let sys = System::new_all();
        sys.processes().values().any(|p| {
            p.name().to_lowercase().contains(needle)
                || p
                    .cmd()
                    .first()
                    .map(|c| c.to_lowercase().contains(needle))
                    .unwrap_or(false)
        })
    };

    let mut appeared = false;
    let mut gone_checks = 0u32;
    for tick in 0u32.. {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if running(&needle) {
            if !appeared {
                info!("{} is running (cxstart handoff complete)", needle);
            }
            appeared = true;
            gone_checks = 0;
        } else if appeared {
            gone_checks += 1;
            if gone_checks >= 2 {
                info!("{} exited", needle);
                break;
            }
        } else if tick > 30 {
            warn!(
                "{} never appeared after cxstart handoff (waited ~60s); treating launch as done",
                needle
            );
            break;
        }
    }

    Ok(String::new())
}

pub fn proton_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/proton"))
}

pub fn wine_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine"))
}

pub fn eac_dir() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/eac_runtime"))
}

pub fn umu_bin() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/umu/umu-run"))
}

fn versions() -> Result<Versions, NativeError> {
    let file = maxima_dir()?.join(VERSION_FILE);
    if !file.exists() {
        return Ok(Versions::default());
    }

    let data = std::fs::read_to_string(file)?;
    Ok(toml::from_str(&data).unwrap_or_default())
}

fn set_versions(versions: Versions) -> Result<(), NativeError> {
    let file = maxima_dir()?.join(VERSION_FILE);
    std::fs::write(file, toml::to_string(&versions)?)?;
    Ok(())
}

pub(crate) async fn check_wine_validity() -> Result<bool, NativeError> {
    if !proton_dir()?.exists() {
        return Ok(false);
    }

    let version = versions()?.proton;

    let release = get_wine_release();
    if let Err(err) = release {
        if !version.is_empty() {
            warn!("Failed to check wine release, rate limited?");
            return Ok(true);
        }

        return Err(NativeError::Wine(err));
    }

    Ok(version == release?.tag_name)
}

pub(crate) async fn get_lutris_runtimes() -> Result<Vec<LutrisRuntime>, WineError> {
    let client = reqwest::Client::builder()
        .user_agent("ArmchairDevelopers/Maxima")
        .build()?;
    let res = client.get("https://lutris.net/api/runtimes").send().await?;
    let res = res.error_for_status()?;
    let data = res.json().await?;
    Ok(data)
}

pub(crate) async fn check_runtime_validity(
    key: &str,
    runtimes: &[LutrisRuntime],
) -> Result<bool, NativeError> {
    let versions = versions()?;
    let version = match key {
        "umu" => &versions.umu,
        "eac_runtime" => &versions.eac_runtime,
        _ => {
            return Err(NativeError::Wine(WineError::UnimplementedRuntime(
                key.to_string(),
            )))
        }
    };
    let path = wine_dir()?.join(key);
    if !path.exists() {
        return Ok(false);
    }
    let runtime_version = runtimes.iter().find(|r| r.name == key);

    Ok(runtime_version.is_some_and(|r| &r.created_at == version))
}

pub(crate) async fn install_runtime(
    key: &str,
    runtimes: &[LutrisRuntime],
) -> Result<(), NativeError> {
    info!("Downloading {key}");
    let runtime = runtimes
        .iter()
        .find(|r| r.name == key)
        .ok_or(NativeError::Wine(WineError::UnimplementedRuntime(
            key.to_string(),
        )))?;
    let mut versions = versions()?;
    let path = wine_dir()?.join(key);
    let runtime_ver = match key {
        "umu" => &mut versions.umu,
        "eac_runtime" => &mut versions.eac_runtime,
        _ => {
            return Err(NativeError::Wine(WineError::UnimplementedRuntime(
                key.to_string(),
            )))
        }
    };

    let res = match ureq::get(&runtime.url)
        .set("User-Agent", "ArmchairDevelopers/Maxima")
        .call()
    {
        Err(err) => return Err(NativeError::Download(DownloadError::Request1(err))),
        Ok(res) => res,
    };

    if res.status() != StatusCode::OK {
        return Err(NativeError::Download(DownloadError::Http(key.to_string())));
    }

    let mut body: Vec<u8> = vec![];
    res.into_reader().read_to_end(&mut body)?;

    if path.exists() {
        remove_dir_all(&path)?;
    }

    create_dir_all(&path)?;

    let data: Box<dyn std::io::Read> = if runtime.url.ends_with(".xz") {
        Box::new(XzDecoder::new(&body[..]))
    } else {
        Box::new(&body[..])
    };

    let archive = Archive::new(data);
    extract_archive(path, archive)?;

    let created_at = runtime.created_at.clone();
    *runtime_ver = created_at;
    set_versions(versions)
}

fn get_wine_release() -> Result<GithubRelease, WineError> {
    let releases = fetch_github_releases("GloriousEggroll", "proton-ge-custom")?;

    let mut release = None;
    for r in releases {
        if r.tag_name.ends_with("LoL") {
            continue;
        }

        release = Some(r);
        break;
    }

    release.ok_or(WineError::Fetch)
}

pub async fn run_wine_command<I: IntoIterator<Item = T>, T: AsRef<OsStr>>(
    arg: T,
    args: Option<I>,
    cwd: Option<PathBuf>,
    want_output: bool,
    command_type: CommandType,
) -> Result<String, NativeError> {
    let proton_path = proton_dir()?;
    let proton_prefix_path = wine_prefix_dir()?;
    let eac_path = eac_dir()?;
    let umu_bin = umu_bin()?;

    // macOS + auto-detected CrossOver + fire-and-monitor commands (games,
    // installers): hand off through cxstart — see run_via_cxstart. A
    // user-set MAXIMA_WINE_COMMAND opts out (their engine, their rules);
    // output-capturing calls (regedit parses, version checks) keep the
    // direct wine invocation, which is fine for non-rendering processes.
    #[cfg(target_os = "macos")]
    if !want_output
        && env::var("MAXIMA_WINE_COMMAND").is_err()
        && std::path::Path::new(CROSSOVER_CXSTART).exists()
    {
        let exe: std::ffi::OsString = arg.as_ref().to_owned();
        let arg_vec: Vec<std::ffi::OsString> = args
            .map(|list| {
                list.into_iter()
                    .map(|a| a.as_ref().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let _ = command_type; // cxstart has no verb concept
        return run_via_cxstart(&proton_prefix_path, exe, arg_vec).await;
    }

    let wine_path = env::var("MAXIMA_WINE_COMMAND").unwrap_or_else(|_| {
        #[cfg(target_os = "macos")]
        if std::path::Path::new(CROSSOVER_WINE).exists() {
            return CROSSOVER_WINE.to_string();
        }
        umu_bin.to_string_lossy().to_string()
    });

    // Create command with all necessary wine env variables
    let mut binding = Command::new(wine_path.clone());
    let mut child = binding
        .env("WINEPREFIX", &proton_prefix_path)
        .env("GAMEID", "umu-0")
        .env("PROTON_VERB", &command_type.to_string())
        .env("PROTONPATH", proton_path)
        .env("STORE", "ea")
        .env("PROTON_EAC_RUNTIME", eac_path)
        .env("UMU_ZENITY", "1")
        .env("WINEDEBUG", "fixme-all")
        .env("LD_PRELOAD", "") // Fixes some log errors for some games
        .arg(arg);

    if !wine_path.ends_with("umu-run") {
        // wsock32 is used as a proxy for Northstar (Titanfall 2). TODO: provide user-facing option for this!
        child = child.env(
            "WINEDLLOVERRIDES",
            "CryptBase,wsock32,bcrypt,dxgi,d3d11,d3d12,d3d12core=n,b;winemenubuilder.exe=d",
        );
    }

    // CrossOver's wine wrapper selects bottles by name (CX_BOTTLE); derive it
    // from the prefix dir so WINEPREFIX and CX_BOTTLE agree. Harmless for
    // non-CrossOver wine, which ignores the variable.
    #[cfg(target_os = "macos")]
    if let Some(bottle) = proton_prefix_path.file_name().and_then(|n| n.to_str()) {
        child = child.env("CX_BOTTLE", bottle);
    }

    if let Some(arguments) = args {
        child = child.args(arguments);
    }

    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }

    let status: ExitStatus;
    let mut output_str = String::new();

    if want_output {
        let output = child
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;
        // Capture both streams. stderr is appended (labelled) so Wine diagnostics
        // surface in WineError::Command instead of being silently dropped.
        output_str = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            if !output_str.is_empty() {
                output_str.push('\n');
            }
            output_str.push_str("[stderr] ");
            output_str.push_str(&stderr_str);
        }
        status = output.status;
    } else {
        // No output wanted → give wine null stdio instead of inheriting.
        // Inherited descriptors from a GUI frontend (JSONL pipes, app fds)
        // reach the game and confuse wine's macOS driver (TF2 freezes after
        // LSX GetAllGameInfo — see launch.rs bootstrap spawn note), and
        // wine's fixme spam would otherwise pollute a parent's stdout
        // protocol. Wine's own logs (CX_LOG / maxima log files) keep the
        // diagnostics.
        status = child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
            .wait()
            .await?;
    };

    if !status.success() {
        return Err(NativeError::Wine(WineError::Command {
            output: output_str,
            exit: status,
        }));
    }

    Ok(output_str.to_string())
}

pub(crate) async fn install_wine() -> Result<(), NativeError> {
    let release = get_wine_release()?;
    let asset = match release
        .assets
        .iter()
        .find(|x| PROTON_PATTERN.captures(&x.name).is_some())
    {
        Some(asset) => asset,
        None => return Err(NativeError::Wine(WineError::Fetch)),
    };

    let dir = maxima_dir()?.join("downloads");
    create_dir_all(&dir)?;

    let path = dir.join(&asset.name);
    github_download_asset(asset, &path)?;
    extract_wine(&path)?;

    let mut versions = versions()?;
    versions.proton = release.tag_name;
    set_versions(versions)?;

    if let Err(err) = remove_file(&path) {
        warn!("Failed to delete {:?} - {:?}", path, err);
    }

    let _ = run_wine_command("", None::<[&str; 0]>, None, false, CommandType::Run).await;

    Ok(())
}

fn extract_wine(archive_path: &PathBuf) -> Result<(), NativeError> {
    info!("Extracting proton...");

    let dir = proton_dir()?;
    if dir.exists() {
        remove_dir_all(&dir)?;
    }

    create_dir_all(&dir)?;

    let archive_file = File::open(archive_path)?;
    let archive_decoder = GzDecoder::new(archive_file);
    let archive = Archive::new(archive_decoder);
    extract_archive(dir, archive)
}

fn extract_archive<R: Read + Sized>(
    dir: PathBuf,
    mut archive: Archive<R>,
) -> Result<(), NativeError> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?;

        let next = match entry_path.components().next() {
            Some(next) => next,
            None => {
                return Err(NativeError::PathComponentNext(entry_path.clone().into()));
            }
        };
        let destination_path = dir.join(entry_path.strip_prefix(next)?);
        if let Some(parent_dir) = destination_path.parent() {
            create_dir_all(parent_dir)?;
        }

        entry.unpack(destination_path)?;
    }

    Ok(())
}

/// Clean up any interrupted WiX Burn-engine installations (vcredist etc.) that
/// left a `/burn.runonce` entry in Wine's RunOnce registry key and a `state.rsm`
/// checkpoint in the Package Cache. A Burn install that is killed mid-run sets
/// `Resume=dword:00000001` in its Uninstall key and adds a RunOnce entry; the
/// next invocation then tries to resume from the (potentially corrupt) checkpoint
/// and exits with code 1 instead of installing fresh.
pub async fn cleanup_interrupted_burn_installs() -> Result<(), NativeError> {
    let registry = parse_mx_wine_registry().await?;

    let runonce_prefix_wow =
        "software\\wow6432node\\microsoft\\windows\\currentversion\\runonce\\";
    let runonce_prefix_plain = "software\\microsoft\\windows\\currentversion\\runonce\\";

    let mut to_clean: Vec<(String, String)> = Vec::new();
    for (full_key, value) in &registry {
        if (!full_key.starts_with(runonce_prefix_wow)
            && !full_key.starts_with(runonce_prefix_plain))
            || !value.contains("burn.runonce")
        {
            continue;
        }
        let value_name = full_key.rsplit('\\').next().unwrap_or("").to_string();
        if let Some(guid) = extract_burn_guid_from_command(value) {
            info!(
                "Found interrupted Burn install: RunOnce='{}' GUID={}",
                value_name, guid
            );
            to_clean.push((value_name, guid));
        }
    }

    if to_clean.is_empty() {
        return Ok(());
    }

    // Delete state.rsm checkpoint files directly on the host filesystem
    let prefix = wine_prefix_dir()?;
    for (_, guid) in &to_clean {
        let state_rsm = prefix
            .join("drive_c")
            .join("ProgramData")
            .join("Package Cache")
            .join(guid)
            .join("state.rsm");
        if state_rsm.exists() {
            match tokio::fs::remove_file(&state_rsm).await {
                Ok(()) => info!("Deleted Burn checkpoint: {}", state_rsm.display()),
                Err(err) => warn!("Could not delete {}: {}", state_rsm.display(), err),
            }
        }
    }

    // Build a .reg file to clear Resume flags and delete RunOnce entries
    let mut reg = "Windows Registry Editor Version 5.00\n\n".to_string();
    for (value_name, guid) in &to_clean {
        for key_prefix in &[
            "HKEY_LOCAL_MACHINE\\Software\\Wow6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
            "HKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        ] {
            reg.push_str(&format!("[{}\\{}]\n", key_prefix, guid));
            reg.push_str("\"Resume\"=dword:00000000\n\n");
        }
        for key_prefix in &[
            "HKEY_LOCAL_MACHINE\\Software\\Wow6432Node\\Microsoft\\Windows\\CurrentVersion\\RunOnce",
            "HKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\RunOnce",
        ] {
            reg.push_str(&format!("[{}]\n", key_prefix));
            // "ValueName"=- is the .reg syntax for deleting a value
            reg.push_str(&format!("\"{}\"=-\n\n", value_name));
        }
    }

    let reg_path = maxima_dir()?.join("temp").join("burn_cleanup.reg");
    tokio::fs::create_dir_all(reg_path.safe_parent()?).await?;
    tokio::fs::write(&reg_path, reg.as_bytes()).await?;

    run_wine_command(
        "regedit",
        Some(vec!["/S", reg_path.safe_str()?]),
        None,
        true,
        CommandType::Run,
    )
    .await?;

    tokio::fs::remove_file(&reg_path).await?;
    invalidate_mx_wine_registry().await;

    info!("Cleaned up {} interrupted Burn installation(s)", to_clean.len());
    Ok(())
}

fn extract_burn_guid_from_command(command: &str) -> Option<String> {
    let start = command.find('{')?;
    let end = command[start..].find('}')? + start + 1;
    let candidate = &command[start..end];
    // A GUID with braces is {xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx} = 38 chars
    if candidate.len() == 38 {
        Some(candidate.to_string())
    } else {
        None
    }
}

pub async fn setup_wine_registry() -> Result<(), NativeError> {
    let mut reg_content = "Windows Registry Editor Version 5.00\n\n".to_string();
    // This supports text values only at the moment
    // if you need a dword - implement it
    let entries: &[(&str, &[(&str, &str)])] = &[
        (
            "HKEY_LOCAL_MACHINE\\Software\\Electronic Arts\\EA Desktop",
            &[("InstallSuccessful", "true")],
        ),
        (
            "HKEY_LOCAL_MACHINE\\Software\\Origin",
            &[
                ("InstallSuccessful", "true"),
                ("ClientPath", "C:/Windows/System32/conhost.exe"),
            ],
        ),
        (
            "HKEY_LOCAL_MACHINE\\Software\\Electronic Arts\\Origin",
            &[
                ("InstallSuccessful", "true"),
                ("ClientPath", "C:/Windows/System32/conhost.exe"),
            ],
        ),
        (
            "HKEY_LOCAL_MACHINE\\Software\\Wow6432Node\\Electronic Arts\\EA Desktop",
            &[("InstallSuccessful", "true")],
        ),
        // The key Origin-era titles actually read: real Origin is a 32-bit
        // app, so on 64-bit Windows its install info lives at the BARE
        // Wow6432Node\Origin (no Electronic Arts\ prefix). TF2 shows
        // "Failed to initialize Origin: The Origin installation couldn't be
        // found [a0020008]" without it. Same key the NSIS installer writes
        // (installer/maxima-setup.nsi, SetRegView 64) for the in-bottle flow.
        (
            "HKEY_LOCAL_MACHINE\\Software\\Wow6432Node\\Origin",
            &[
                ("InstallSuccessful", "true"),
                ("ClientPath", "C:/Windows/System32/conhost.exe"),
            ],
        ),
        (
            "HKEY_LOCAL_MACHINE\\Software\\Wow6432Node\\Electronic Arts\\Origin",
            &[
                ("InstallSuccessful", "true"),
                ("ClientPath", "C:/Windows/System32/conhost.exe"),
            ],
        ),
    ];

    for (key, values) in entries.into_iter() {
        reg_content.push_str(&format!("[{}]\n", key));
        for (name, value) in values.into_iter() {
            let value = value.replace("\\", "\\\\");
            reg_content.push_str(&format!("\"{}\"=\"{}\"\n\n", name, value));
        }
    }

    // macOS: route Maxima's URL protocols OUT of the bottle to the host.
    // Fresh (native-mode) bottles have no in-bottle maxima-bootstrap.exe;
    // registering winebrowser.exe for the schemes makes wine hand the URL
    // to the host's `open`, where LaunchServices resolves it to the
    // registered MaximaBootstrap.app (see registry::set_up_registry). This
    // is how an externally-launched game's link2ea:// reaches the host
    // auth server / maxima-cli. Linux keeps upstream's flow untouched.
    #[cfg(target_os = "macos")]
    for (protocol, name) in [
        ("link2ea", "Maxima Launcher"),
        ("origin2", "Maxima Launcher"),
        ("qrc", "Maxima Protocol"),
    ] {
        reg_content.push_str(&format!("[HKEY_CLASSES_ROOT\\{}]\n", protocol));
        reg_content.push_str(&format!("@=\"URL:{}\"\n", name));
        reg_content.push_str("\"URL Protocol\"=\"\"\n\n");
        reg_content.push_str(&format!(
            "[HKEY_CLASSES_ROOT\\{}\\shell\\open\\command]\n",
            protocol
        ));
        reg_content.push_str(
            "@=\"C:\\\\windows\\\\system32\\\\winebrowser.exe \\\"%1\\\"\"\n\n",
        );
    }

    let path = maxima_dir()?.join("temp").join("wine.reg");
    tokio::fs::create_dir_all(path.safe_parent()?).await?;

    {
        let mut reg_file = tokio::fs::File::create(&path).await?;
        reg_file.write_all(reg_content.as_bytes()).await?;
    }

    run_wine_command(
        "regedit",
        Some(vec!["/S", path.safe_str()?]),
        None,
        true,
        CommandType::Run,
    )
    .await?;

    tokio::fs::remove_file(path).await?;

    Ok(())
}

pub type WineRegistry = HashMap<String, String>;

lazy_static! {
    static ref MX_WINE_REGISTRY: Mutex<WineRegistry> = Mutex::new(WineRegistry::new());
}

async fn parse_wine_registry(file_path: &str) -> WineRegistry {
    let mut registry_map = MX_WINE_REGISTRY.lock().await;
    if !registry_map.is_empty() {
        return registry_map.clone();
    }

    let file = tokio::fs::File::open(file_path)
        .await
        .expect("Could not open file");
    let reader = BufReader::new(file);
    let mut current_section = String::new();

    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await.expect("Failed to read file") {
        let trimmed_line = line.trim();

        if trimmed_line.starts_with('[') && trimmed_line.contains(']') {
            if let Some(end) = trimmed_line.find(']') {
                current_section = trimmed_line[1..end].to_string();
            }
        } else if trimmed_line.contains('=') && trimmed_line.starts_with('"') {
            let parts: Vec<&str> = trimmed_line.splitn(2, '=').collect();
            if parts.len() == 2 {
                let key = parts[0].trim_matches('"').to_string();
                let value = parts[1].trim_matches('"').to_string();
                let full_key = format!("{}\\{}", current_section, key).replace("\\\\", "\\");
                registry_map.insert(full_key.to_lowercase(), value);
            }
        }
    }

    registry_map.clone()
}

pub async fn parse_mx_wine_registry() -> Result<WineRegistry, NativeError> {
    let path = wine_prefix_dir()?.join("system.reg");
    if !path.exists() {
        return Ok(HashMap::new());
    }

    Ok(parse_wine_registry(path.safe_str()?).await)
}

pub async fn invalidate_mx_wine_registry() {
    MX_WINE_REGISTRY.lock().await.clear();
}

fn normalize_key(key: &str) -> String {
    let lower_key = key.to_lowercase();
    if lower_key.starts_with("hkey_local_machine\\") {
        lower_key
            .trim_start_matches("hkey_local_machine\\")
            .to_string()
    } else {
        lower_key
    }
}

pub async fn get_mx_wine_registry_value(query_key: &str) -> Result<Option<String>, RegistryError> {
    let registry_map = parse_mx_wine_registry().await?;
    let normalized_query_key = normalize_key(query_key);

    let value = if let Some(value) = registry_map.get(&normalized_query_key) {
        Some(value.clone())
    } else {
        let wow6432_query_key =
            normalized_query_key.replace("software\\", "software\\wow6432node\\");
        registry_map.get(&wow6432_query_key).cloned()
    };

    Ok(value.map(|x| x.replace("Z:", "").replace("\\", "/")))
}
