use std::{
    env,
    fs::create_dir_all,
    num::ParseIntError,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[cfg(windows)]
use std::{
    ffi::{CString, OsString},
    os::windows::prelude::{OsStrExt, OsStringExt},
};

#[cfg(windows)]
use winapi::{
    shared::windef::HWND,
    um::{
        libloaderapi::{GetModuleFileNameW, GetModuleHandleW},
        wincon::GetConsoleWindow,
        winuser::{
            EnumWindows, FindWindowA, GetWindowThreadProcessId, IsWindowVisible,
            SetForegroundWindow,
        },
    },
};

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Request1(#[from] ureq::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),

    #[error("failed to download: `{0}`")]
    Http(String),
}

#[derive(Error, Debug)]
pub enum WineError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Download(#[from] DownloadError),

    #[error("failed to run wine command: {output} ({exit:?})")]
    Command {
        output: String,
        exit: std::process::ExitStatus,
    },
    #[error("could not find runtime `{0}`")]
    MissingRuntime(String),
    #[error("runtime `{0}` is not implemented")]
    UnimplementedRuntime(String),
    #[error("couldn't find suitable wine release")]
    Fetch,
}
pub trait SafeParent {
    fn safe_parent(&self) -> Result<&Path, NativeError>;
}

pub trait SafeStr {
    fn safe_str(&self) -> Result<&str, NativeError>;
}

impl SafeParent for PathBuf {
    fn safe_parent(&self) -> Result<&Path, NativeError> {
        match self.parent() {
            Some(parent) => Ok(parent),
            None => Err(NativeError::Parent(self.safe_str()?.to_owned())),
        }
    }
}
impl SafeStr for Path {
    fn safe_str(&self) -> Result<&str, NativeError> {
        self.to_str()
            .ok_or(NativeError::StringifyPath(Box::from(self)))
    }
}

impl SafeParent for Path {
    fn safe_parent(&self) -> Result<&Path, NativeError> {
        self.parent()
            .ok_or(NativeError::Parent(self.safe_str()?.to_owned()))
    }
}

#[derive(Error, Debug)]
pub enum NativeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Wine(#[from] WineError),
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error(transparent)]
    ParseInt(#[from] ParseIntError),

    #[error("missing `{0}` environment variable")]
    MissingEnvironmentVariable(String),
    #[error("could not get the parent directory of `{0:?}`")]
    Parent(String),
    #[error("could not convert `&str` to a `String`")]
    Stringify,
    #[error("could not convert `{0:?}` to a string")]
    StringifyPath(Box<Path>),
    #[error("could not get file name from path")]
    FileName,
    #[error("could not get the next path component of `{0}`")]
    PathComponentNext(Box<Path>),
    #[error("could not find PID of `{0}`")]
    Pid(String),
    #[error("could not find PID pattern")]
    PidPattern,
    #[error(
        "CrossOver not found at /Applications/CrossOver.app — install CrossOver, \
         or set MAXIMA_WINE_COMMAND and MAXIMA_WINE_PREFIX to use a different wine"
    )]
    CrossOverMissing,

    // Windows
    #[error("failed to elevate `{0}`")]
    Elevation(String),
    #[error("could not get module file name")]
    CantFindModuleFileName,
    #[error("could not open process")]
    CantFindProcess,
    #[error("could not find window")]
    CantFindWindow,
    #[error("could not run command. exit code `{0}`")]
    Command(i32),
}

#[cfg(windows)]
unsafe extern "system" fn enum_windows_proc(
    hwnd: HWND,
    _l_param: winapi::shared::minwindef::LPARAM,
) -> winapi::shared::minwindef::BOOL {
    let mut window_process_id: u32 = 0;

    GetWindowThreadProcessId(hwnd, &mut window_process_id);

    if window_process_id != std::process::id() || IsWindowVisible(hwnd) == 0 {
        return winapi::shared::minwindef::TRUE;
    }

    if IsWindowVisible(hwnd) != 0 {
        SetForegroundWindow(hwnd);
    }

    winapi::shared::minwindef::TRUE
}
#[cfg(windows)]
pub fn get_hwnd() -> Result<HWND, NativeError> {
    unsafe {
        EnumWindows(Some(enum_windows_proc), 0);

        let window_name = CString::new("Maxima").expect("Failed to create native string");
        let mut hwnd = FindWindowA(std::ptr::null(), window_name.as_ptr());
        if !hwnd.is_null() {
            return Ok(hwnd);
        }

        hwnd = GetConsoleWindow();
        if hwnd.is_null() {
            return Err(NativeError::CantFindWindow);
        }

        Ok(hwnd)
    }
}

#[cfg(windows)]
pub fn take_foreground_focus() -> Result<(), NativeError> {
    unsafe {
        EnumWindows(Some(enum_windows_proc), 0);
    }

    Ok(())
}

#[cfg(unix)]
pub fn take_foreground_focus() -> Result<(), NativeError> {
    // TODO
    Ok(())
}

#[cfg(windows)]
pub fn module_path() -> Result<PathBuf, NativeError> {
    // Get a handle to the DLL
    let mut maxima_mod_name = OsString::from("maxima.dll")
        .encode_wide()
        .collect::<Vec<_>>();
    maxima_mod_name.push(0);

    let mut hmodule = unsafe { GetModuleHandleW(maxima_mod_name.as_mut_ptr()) };
    if hmodule.is_null() {
        hmodule = unsafe { GetModuleHandleW(std::ptr::null_mut()) };
    }

    if hmodule.is_null() {
        panic!("Failed to find module");
    }

    // Create a buffer to hold the DLL path
    let mut buffer: [u16; 260] = [0; 260];

    // Get the DLL path
    let length = unsafe { GetModuleFileNameW(hmodule, buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        panic!("Failed to get module length");
    }

    // Convert buffer to a Rust String
    let os_string = OsString::from_wide(&buffer[0..length as usize]);
    Ok(os_string.to_string_lossy().into_owned().into())
}

#[cfg(target_os = "linux")]
pub fn module_path() -> Result<PathBuf, NativeError> {
    let path = std::fs::read_link("/proc/self/exe");

    Ok(path?)
}

#[cfg(target_os = "macos")]
pub fn module_path() -> Result<PathBuf, NativeError> {
    Ok(env::current_exe()?)
}

/// Reverse-DNS pieces handed to `ProjectDirs`. Every frontend (CLI, server, TUI,
/// egui UI) resolves its on-disk locations from these, so they all share one
/// data dir and one cache dir on every OS.
pub const APP_QUALIFIER: &str = "com";
pub const APP_ORGANIZATION: &str = "ArmchairDevelopers";
pub const APP_NAME: &str = "Maxima";
/// `com.ArmchairDevelopers.Maxima` — also used as the egui window app id and
/// `.desktop` file stem so desktop environments match the window to its entry.
pub const APP_ID: &str = "com.ArmchairDevelopers.Maxima";

fn project_dirs() -> Result<directories::ProjectDirs, NativeError> {
    directories::ProjectDirs::from(APP_QUALIFIER, APP_ORGANIZATION, APP_NAME).ok_or_else(|| {
        NativeError::MissingEnvironmentVariable(
            if cfg!(windows) { "USERPROFILE" } else { "HOME" }.to_string(),
        )
    })
}

/// Persistent data directory, without creating it: auth tokens, `config.json`,
/// wine/umu runtimes, the download queue, installed helper binaries.
///
/// | OS      | Path                                                              |
/// |---------|-------------------------------------------------------------------|
/// | Linux   | `$XDG_DATA_HOME/maxima` (`~/.local/share/maxima`)                 |
/// | macOS   | `~/Library/Application Support/com.ArmchairDevelopers.Maxima`     |
/// | Windows | `%APPDATA%\ArmchairDevelopers\Maxima\data`                       |
///
/// `MAXIMA_DATA_DIR` replaces it wholesale, which makes a separate, fully
/// independent Maxima instance (its own login and its own server).
pub fn maxima_data_path() -> Result<PathBuf, NativeError> {
    if let Some(dir) = env::var_os("MAXIMA_DATA_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(project_dirs()?.data_dir().to_path_buf())
}

/// Disposable cache directory, without creating it: manifest cache, avatar and
/// UI image caches, in-flight download resume state, downloaded archives and
/// temp files. Safe for the OS (or the user) to delete at any time.
///
/// | OS      | Path                                                              |
/// |---------|-------------------------------------------------------------------|
/// | Linux   | `$XDG_CACHE_HOME/maxima` (`~/.cache/maxima`)                      |
/// | macOS   | `~/Library/Caches/com.ArmchairDevelopers.Maxima`                  |
/// | Windows | `%LOCALAPPDATA%\ArmchairDevelopers\Maxima\cache`                 |
pub fn maxima_cache_path() -> Result<PathBuf, NativeError> {
    Ok(project_dirs()?.cache_dir().to_path_buf())
}

/// Log directory, without creating it. Windows keeps the long-documented
/// `%LOCALAPPDATA%\Maxima\Logs` (consumers inspect it inside CrossOver
/// bottles); elsewhere logs live under the data dir.
pub fn maxima_logs_path() -> Result<PathBuf, NativeError> {
    #[cfg(windows)]
    {
        env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("APPDATA"))
            .map(|p| PathBuf::from(p).join("Maxima").join("Logs"))
            .ok_or_else(|| NativeError::MissingEnvironmentVariable("LOCALAPPDATA".to_string()))
    }
    #[cfg(not(windows))]
    {
        Ok(maxima_data_path()?.join("logs"))
    }
}

pub fn maxima_dir() -> Result<PathBuf, NativeError> {
    let path = maxima_data_path()?;
    create_dir_all(&path)?;
    Ok(path)
}

pub fn maxima_cache_dir() -> Result<PathBuf, NativeError> {
    let path = maxima_cache_path()?;
    create_dir_all(&path)?;
    Ok(path)
}

pub fn maxima_logs_dir() -> Result<PathBuf, NativeError> {
    let path = maxima_logs_path()?;
    create_dir_all(&path)?;
    Ok(path)
}

#[cfg(unix)]
pub fn platform_path<P: AsRef<Path>>(path: P) -> PathBuf {
    PathBuf::from(format!("Z:{}", path.as_ref().to_str().unwrap()))
}

#[cfg(windows)]
pub fn platform_path<P: AsRef<Path>>(path: P) -> PathBuf {
    PathBuf::from(path.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_path(var: &str) -> Option<PathBuf> {
        env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    fn home() -> Option<PathBuf> {
        env_path("HOME")
    }

    #[test]
    fn data_path_matches_platform_convention() {
        #[cfg(target_os = "linux")]
        {
            let base = env_path("XDG_DATA_HOME")
                .filter(|p| p.is_absolute())
                .or_else(|| home().map(|h| h.join(".local/share")));
            if let Some(base) = base {
                assert_eq!(maxima_data_path().unwrap(), base.join("maxima"));
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(h) = home() {
            assert_eq!(
                maxima_data_path().unwrap(),
                h.join("Library/Application Support/com.ArmchairDevelopers.Maxima")
            );
        }
        #[cfg(windows)]
        if let Some(a) = env_path("APPDATA") {
            assert_eq!(
                maxima_data_path().unwrap(),
                a.join("ArmchairDevelopers").join("Maxima").join("data")
            );
        }
    }

    #[test]
    fn cache_path_matches_platform_convention() {
        #[cfg(target_os = "linux")]
        {
            let base = env_path("XDG_CACHE_HOME")
                .filter(|p| p.is_absolute())
                .or_else(|| home().map(|h| h.join(".cache")));
            if let Some(base) = base {
                assert_eq!(maxima_cache_path().unwrap(), base.join("maxima"));
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(h) = home() {
            assert_eq!(
                maxima_cache_path().unwrap(),
                h.join("Library/Caches/com.ArmchairDevelopers.Maxima")
            );
        }
        #[cfg(windows)]
        if let Some(a) = env_path("LOCALAPPDATA") {
            assert_eq!(
                maxima_cache_path().unwrap(),
                a.join("ArmchairDevelopers").join("Maxima").join("cache")
            );
        }
    }

    #[test]
    fn data_and_cache_are_distinct_and_logs_are_not_in_cache() {
        let (Ok(data), Ok(cache), Ok(logs)) =
            (maxima_data_path(), maxima_cache_path(), maxima_logs_path())
        else {
            return;
        };
        assert_ne!(data, cache);
        assert!(!logs.starts_with(&cache));
        #[cfg(not(windows))]
        assert_eq!(logs, data.join("logs"));
    }

    #[test]
    fn app_id_is_the_reverse_dns_of_the_project_dirs_triple() {
        assert_eq!(
            APP_ID,
            format!("{APP_QUALIFIER}.{APP_ORGANIZATION}.{APP_NAME}")
        );
    }
}
