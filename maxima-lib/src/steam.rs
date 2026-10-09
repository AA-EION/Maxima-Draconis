//! Steam install discovery and per-game overrides, shared between the HTTP
//! `/authorize` handler (which receives Steam App IDs from `link2ea://`) and
//! the launch path.
//!
//! Nothing here knows about a particular game. A Steam App ID is tied to an
//! EA offer through, in order:
//!
//! 1. the optional overrides file `game-overrides.json` in Maxima's data
//!    directory (or the path in `MAXIMA_GAME_OVERRIDES`): a JSON array of
//!    `{"offer_id": "...", "steam_app_id": "...", "exe": "...",
//!    "install_dir": "..."}` objects, every key but `offer_id` optional;
//! 2. Steam's own `appmanifest_<appid>.acf` files, whose `name` is matched
//!    against the display names in the user's EA library.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use lazy_static::lazy_static;
use log::{debug, warn};
use regex::Regex;
use serde::Deserialize;

use crate::util::native::maxima_data_path;

lazy_static! {
    /// Matches a well-formed EA offer ID like "Origin.OFR.50.0002694".
    pub static ref EA_OFFER_ID_PATTERN: Regex = Regex::new(r"^Origin\.OFR\.\d+\.\d+$").unwrap();
    /// Matches a Steam App ID emitted by `link2ea://launchgame/<id>?platform=steam`.
    /// Current Steam App IDs fit in 1..=10 ASCII digits (max issued is ~3M).
    pub static ref STEAM_APP_ID_PATTERN: Regex = Regex::new(r"^\d{1,10}$").unwrap();
}

pub const GAME_OVERRIDES_ENV: &str = "MAXIMA_GAME_OVERRIDES";
const GAME_OVERRIDES_FILE: &str = "game-overrides.json";

/// One entry of the per-game overrides file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GameOverride {
    pub offer_id: String,
    #[serde(default)]
    pub steam_app_id: Option<String>,
    /// Executable file name (or path relative to the install directory).
    #[serde(default)]
    pub exe: Option<String>,
    /// Install directory: absolute, or a folder name under `steamapps/common`.
    #[serde(default)]
    pub install_dir: Option<String>,
}

/// Parses the overrides file leniently: malformed input or entries are
/// logged and skipped, never an error.
pub fn parse_game_overrides(json: &str) -> Vec<GameOverride> {
    let values: Vec<serde_json::Value> = match serde_json::from_str(json) {
        Ok(values) => values,
        Err(err) => {
            warn!("Ignoring game overrides: not a JSON array ({})", err);
            return Vec::new();
        }
    };

    values
        .into_iter()
        .filter_map(|value| match serde_json::from_value::<GameOverride>(value) {
            Ok(entry) if !entry.offer_id.trim().is_empty() => Some(entry),
            Ok(_) => {
                warn!("Ignoring game override with an empty offer_id");
                None
            }
            Err(err) => {
                warn!("Ignoring malformed game override: {}", err);
                None
            }
        })
        .collect()
}

pub fn game_overrides_path() -> Option<PathBuf> {
    match std::env::var_os(GAME_OVERRIDES_ENV) {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => maxima_data_path().ok().map(|dir| dir.join(GAME_OVERRIDES_FILE)),
    }
}

/// Loads the overrides file; absent or unreadable means no overrides.
pub fn load_game_overrides() -> Vec<GameOverride> {
    let Some(path) = game_overrides_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_game_overrides(&content),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!("Could not read {}: {}", path.display(), err);
            }
            Vec::new()
        }
    }
}

pub fn override_for_offer<'a>(
    overrides: &'a [GameOverride],
    offer_id: &str,
) -> Option<&'a GameOverride> {
    overrides.iter().find(|o| o.offer_id == offer_id)
}

pub fn override_for_steam_app<'a>(
    overrides: &'a [GameOverride],
    steam_app_id: &str,
) -> Option<&'a GameOverride> {
    overrides
        .iter()
        .find(|o| o.steam_app_id.as_deref() == Some(steam_app_id))
}

/// The fields of a Steam `appmanifest_<appid>.acf` Maxima cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamAppManifest {
    pub appid: String,
    pub name: String,
    pub installdir: String,
}

/// Every `"key" "value"` pair on a line of a Valve KeyValues file, with the
/// value unescaped. Nesting is ignored; keys are lowercased.
fn key_value_pairs(content: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in content.lines() {
        let mut quoted = Vec::new();
        let mut chars = line.trim().chars();
        while let Some(c) = chars.next() {
            if c != '"' {
                continue;
            }
            let mut token = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some('n') => token.push('\n'),
                        Some('t') => token.push('\t'),
                        Some(other) => token.push(other),
                        None => break,
                    },
                    '"' => break,
                    other => token.push(other),
                }
            }
            quoted.push(token);
        }
        if let [key, value] = quoted.as_slice() {
            pairs.push((key.to_lowercase(), value.clone()));
        }
    }
    pairs
}

/// Parses an `appmanifest_<appid>.acf`. `None` when `appid` or `installdir`
/// is missing; a missing `name` falls back to the install directory name.
pub fn parse_app_manifest(content: &str) -> Option<SteamAppManifest> {
    let (mut appid, mut name, mut installdir) = (None, None, None);
    for (key, value) in key_value_pairs(content) {
        match key.as_str() {
            "appid" if appid.is_none() => appid = Some(value),
            "name" if name.is_none() => name = Some(value),
            "installdir" if installdir.is_none() => installdir = Some(value),
            _ => {}
        }
    }
    let installdir = installdir.filter(|d| !d.is_empty())?;
    Some(SteamAppManifest {
        appid: appid.filter(|a| STEAM_APP_ID_PATTERN.is_match(a))?,
        name: name.unwrap_or_else(|| installdir.clone()),
        installdir,
    })
}

/// The `"path"` entries of a `libraryfolders.vdf`.
pub fn parse_library_paths(content: &str) -> Vec<String> {
    key_value_pairs(content)
        .into_iter()
        .filter(|(key, _)| key == "path")
        .map(|(_, value)| value)
        .collect()
}

/// Lowercased alphanumerics only, so "Some Game: Deluxe!" and
/// "some-game deluxe" compare equal.
pub fn normalize_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The offer whose name matches `steam_name`, if exactly one offer does.
/// `offers` pairs an offer id with the display names it is known by.
pub fn match_offer_by_name(steam_name: &str, offers: &[(String, Vec<String>)]) -> Option<String> {
    let wanted = normalize_name(steam_name);
    if wanted.is_empty() {
        return None;
    }

    let matching: HashSet<&String> = offers
        .iter()
        .filter(|(_, names)| names.iter().any(|name| normalize_name(name) == wanted))
        .map(|(offer_id, _)| offer_id)
        .collect();

    match matching.len() {
        1 => matching.into_iter().next().cloned(),
        _ => None,
    }
}

/// Where a Windows-style path (`C:\Games\x`) lives on this host. On Windows
/// it is returned unchanged; elsewhere only drive `C:` can be mapped, into
/// the Wine prefix, and a path that is already a host path passes through.
fn host_path(raw: &str, wine_prefix: Option<&Path>) -> Option<PathBuf> {
    let bytes = raw.as_bytes();
    let has_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if cfg!(windows) || !has_drive {
        return Some(PathBuf::from(raw));
    }
    if !raw[..1].eq_ignore_ascii_case("c") {
        return None;
    }
    let relative = raw[2..].replace('\\', "/");
    Some(wine_prefix?.join("drive_c").join(relative.trim_start_matches('/')))
}

/// Steam installation roots: the registry and the default locations on
/// Windows, the default locations inside `wine_prefix` elsewhere.
pub fn steam_roots(wine_prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.exists() && !roots.contains(&p) {
            roots.push(p);
        }
    };

    #[cfg(windows)]
    {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        for key in &["SOFTWARE\\WOW6432Node\\Valve\\Steam", "SOFTWARE\\Valve\\Steam"] {
            if let Ok(subkey) = hklm.open_subkey(key) {
                if let Ok(path) = subkey.get_value::<String, _>("InstallPath") {
                    push(PathBuf::from(path));
                }
            }
        }
    }

    for default in ["Program Files (x86)/Steam", "Program Files/Steam"] {
        match wine_prefix {
            Some(prefix) if !cfg!(windows) => push(prefix.join("drive_c").join(default)),
            _ if cfg!(windows) => push(PathBuf::from("C:\\").join(default.replace('/', "\\"))),
            _ => {}
        }
    }

    roots
}

/// Steam library folders (each holds a `steamapps` directory), the Steam
/// roots included.
pub fn steam_libraries(wine_prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut libraries = Vec::new();
    for root in steam_roots(wine_prefix) {
        if !libraries.contains(&root) {
            libraries.push(root.clone());
        }
        for vdf in [
            root.join("steamapps").join("libraryfolders.vdf"),
            root.join("config").join("libraryfolders.vdf"),
        ] {
            let Ok(content) = std::fs::read_to_string(&vdf) else {
                continue;
            };
            for raw in parse_library_paths(&content) {
                match host_path(&raw, wine_prefix) {
                    Some(path) if path.exists() && !libraries.contains(&path) => {
                        libraries.push(path)
                    }
                    Some(_) => {}
                    None => debug!("Steam library '{}' is not reachable from this host", raw),
                }
            }
        }
    }
    libraries
}

/// An installed Steam app found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSteamApp {
    pub manifest: SteamAppManifest,
    /// The library folder holding the app's `steamapps` directory.
    pub library: PathBuf,
}

impl InstalledSteamApp {
    pub fn install_dir(&self) -> PathBuf {
        self.library
            .join("steamapps")
            .join("common")
            .join(&self.manifest.installdir)
    }
}

/// Every app with an `appmanifest_*.acf` in any Steam library.
pub fn installed_steam_apps(wine_prefix: Option<&Path>) -> Vec<InstalledSteamApp> {
    let mut apps = Vec::new();
    for library in steam_libraries(wine_prefix) {
        let Ok(entries) = std::fs::read_dir(library.join("steamapps")) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !(file_name.starts_with("appmanifest_") && file_name.ends_with(".acf")) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            match parse_app_manifest(&content) {
                Some(manifest) => apps.push(InstalledSteamApp {
                    manifest,
                    library: library.clone(),
                }),
                None => debug!("Skipping unreadable {}", entry.path().display()),
            }
        }
    }
    apps
}

pub fn installed_steam_app(
    steam_app_id: &str,
    wine_prefix: Option<&Path>,
) -> Option<InstalledSteamApp> {
    installed_steam_apps(wine_prefix)
        .into_iter()
        .find(|app| app.manifest.appid == steam_app_id)
}

/// The install directory named by an override: absolute, or a folder name
/// under `steamapps/common` of any known library.
pub fn override_install_dir(
    install_dir: &str,
    wine_prefix: Option<&Path>,
) -> Option<PathBuf> {
    let direct = host_path(install_dir, wine_prefix)?;
    if direct.is_absolute() || install_dir.contains(':') {
        return Some(direct);
    }
    steam_libraries(wine_prefix)
        .into_iter()
        .map(|library| library.join("steamapps").join("common").join(install_dir))
        .find(|dir| dir.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACF: &str = r#"
"AppState"
{
	"appid"		"12345"
	"Universe"		"1"
	"name"		"Some Game: Deluxe \"Edition\""
	"StateFlags"		"4"
	"installdir"		"SomeGame"
	"UserConfig"
	{
		"language"		"english"
	}
}
"#;

    #[test]
    fn parses_app_manifest() {
        let manifest = parse_app_manifest(ACF).unwrap();
        assert_eq!(manifest.appid, "12345");
        assert_eq!(manifest.name, "Some Game: Deluxe \"Edition\"");
        assert_eq!(manifest.installdir, "SomeGame");
    }

    #[test]
    fn app_manifest_without_required_keys_is_rejected() {
        assert_eq!(parse_app_manifest(r#""AppState" { "appid" "1" }"#), None);
        assert_eq!(
            parse_app_manifest(r#""AppState" { "installdir" "x" "name" "n" }"#),
            None
        );
        assert_eq!(
            parse_app_manifest("\"appid\" \"not-a-number\"\n\"installdir\" \"x\"\n"),
            None
        );
    }

    #[test]
    fn app_manifest_name_falls_back_to_install_dir() {
        let manifest = parse_app_manifest("\"appid\" \"7\"\n\"installdir\" \"Folder\"\n").unwrap();
        assert_eq!(manifest.name, "Folder");
    }

    #[test]
    fn parses_library_paths() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        assert_eq!(
            parse_library_paths(vdf),
            vec![
                r"C:\Program Files (x86)\Steam".to_string(),
                r"D:\SteamLibrary".to_string()
            ]
        );
    }

    #[test]
    fn names_are_compared_loosely() {
        assert_eq!(normalize_name("Some Game: Deluxe!"), "somegamedeluxe");
        let offers = vec![
            ("Origin.OFR.1.1".to_string(), vec!["Some-Game Deluxe".to_string()]),
            ("Origin.OFR.2.2".to_string(), vec!["Other Game".to_string()]),
        ];
        assert_eq!(
            match_offer_by_name("some game: DELUXE", &offers),
            Some("Origin.OFR.1.1".to_string())
        );
        assert_eq!(match_offer_by_name("Unknown", &offers), None);
        assert_eq!(match_offer_by_name("!!!", &offers), None);
    }

    #[test]
    fn ambiguous_names_do_not_match() {
        let offers = vec![
            ("Origin.OFR.1.1".to_string(), vec!["Same".to_string()]),
            ("Origin.OFR.2.2".to_string(), vec!["same".to_string()]),
        ];
        assert_eq!(match_offer_by_name("Same", &offers), None);
    }

    #[test]
    fn overrides_load_leniently() {
        let json = r#"[
            {"offer_id": "Origin.OFR.1.1", "steam_app_id": "42", "exe": "game.exe", "install_dir": "Game"},
            {"offer_id": "Origin.OFR.2.2"},
            {"steam_app_id": "7"},
            {"offer_id": "  "},
            {"offer_id": 5},
            "nonsense"
        ]"#;
        let overrides = parse_game_overrides(json);
        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0].steam_app_id.as_deref(), Some("42"));
        assert_eq!(overrides[0].exe.as_deref(), Some("game.exe"));
        assert_eq!(overrides[1].install_dir, None);

        assert_eq!(
            override_for_steam_app(&overrides, "42").map(|o| o.offer_id.as_str()),
            Some("Origin.OFR.1.1")
        );
        assert!(override_for_steam_app(&overrides, "43").is_none());
        assert!(override_for_offer(&overrides, "Origin.OFR.2.2").is_some());
    }

    #[test]
    fn overrides_that_are_not_an_array_are_ignored() {
        assert!(parse_game_overrides("{}").is_empty());
        assert!(parse_game_overrides("not json").is_empty());
        assert!(parse_game_overrides("").is_empty());
        assert!(parse_game_overrides("[]").is_empty());
    }

    #[test]
    fn overrides_file_comes_from_the_environment() {
        let dir = std::env::temp_dir().join(format!("maxima-overrides-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("custom.json");
        std::fs::write(&file, r#"[{"offer_id": "Origin.OFR.9.9", "steam_app_id": "9"}]"#).unwrap();

        std::env::set_var(GAME_OVERRIDES_ENV, &file);
        let loaded = load_game_overrides();
        std::env::set_var(GAME_OVERRIDES_ENV, dir.join("missing.json"));
        let missing = load_game_overrides();
        std::env::remove_var(GAME_OVERRIDES_ENV);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].offer_id, "Origin.OFR.9.9");
        assert!(missing.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn finds_installed_apps_inside_a_prefix() {
        let prefix = std::env::temp_dir().join(format!("maxima-steam-{}", std::process::id()));
        let steamapps = prefix.join("drive_c/Program Files (x86)/Steam/steamapps");
        std::fs::create_dir_all(steamapps.join("common/SomeGame")).unwrap();
        std::fs::write(steamapps.join("appmanifest_12345.acf"), ACF).unwrap();
        std::fs::write(steamapps.join("appmanifest_bad.acf"), "garbage").unwrap();

        let found = installed_steam_app("12345", Some(&prefix));
        let none = installed_steam_app("999", Some(&prefix));
        let dir = override_install_dir("SomeGame", Some(&prefix));
        let absolute = override_install_dir(r"C:\Games\Other", Some(&prefix));
        let _ = std::fs::remove_dir_all(&prefix);

        let app = found.unwrap();
        assert_eq!(app.manifest.installdir, "SomeGame");
        assert!(app.install_dir().ends_with("steamapps/common/SomeGame"));
        assert!(none.is_none());
        assert!(dir.unwrap().ends_with("steamapps/common/SomeGame"));
        assert!(absolute.unwrap().ends_with("drive_c/Games/Other"));
    }
}
