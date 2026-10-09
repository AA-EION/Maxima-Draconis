//! Per-game install records, independent of any Wine prefix.
//!
//! A game's install location and (on unix) the Wine prefix it runs in used to
//! be rediscovered on every call by reading the prefix's registry, which tied
//! "is this game installed?" to a single ambient prefix. The record written
//! here is the source of truth instead: `<data dir>/gameinfo/<slug>.json`,
//! the same layout upstream Maxima uses, so the two stay file-compatible.
//!
//! Upstream only stores `path` and `wine_prefix` (with `""` meaning "no
//! prefix"). The extra fields below are all `#[serde(default)]`, so a record
//! written by upstream still loads and ours still loads upstream.

use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
use log::warn;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::util::native::{maxima_dir, NativeError};

pub const GAMEINFO_DIR: &str = "gameinfo";

#[derive(Error, Debug)]
pub enum GameInfoError {
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("no install record for `{0}`")]
    NotFound(String),
    #[error("`{0}` is not a valid game slug")]
    InvalidSlug(String),
}

/// `None` <-> `""` on the wire (upstream's convention); `null` and a missing
/// field read as `None` too.
fn prefix_from_string<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|s| !s.is_empty()).map(PathBuf::from))
}

fn prefix_to_string<S>(value: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        None => serializer.serialize_str(""),
        Some(path) => serializer.serialize_str(&path.to_string_lossy()),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GameInstallInfo {
    /// The game's install directory.
    pub path: PathBuf,
    /// The Wine prefix / CrossOver bottle the game runs in. `None` on native
    /// Windows, or when the prefix was left to the platform default.
    #[serde(
        default,
        deserialize_with = "prefix_from_string",
        serialize_with = "prefix_to_string"
    )]
    pub wine_prefix: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    /// Version string from the installed game's own manifest, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Locale the game was installed / last launched with (e.g. `en_US`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// Glob patterns excluded from the download set and from verify; merged
    /// with the per-game exclusion file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// RFC 3339 UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl GameInstallInfo {
    pub fn new(path: PathBuf, wine_prefix: Option<PathBuf>) -> Self {
        Self {
            path,
            wine_prefix,
            ..Default::default()
        }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn wine_prefix(&self) -> Option<PathBuf> {
        self.wine_prefix.clone()
    }

    pub fn with_slug(mut self, slug: &str) -> Self {
        self.slug = Some(slug.to_owned());
        self
    }

    pub fn with_offer(mut self, offer_id: &str, build_id: Option<&str>) -> Self {
        self.offer_id = Some(offer_id.to_owned());
        self.build_id = build_id.map(str::to_owned);
        self
    }

    pub fn with_locale(mut self, locale: &str) -> Self {
        self.locale = Some(locale.to_owned());
        self
    }

    pub fn with_exclude(mut self, exclude: Vec<String>) -> Self {
        self.exclude = exclude;
        self
    }

    /// Fill the timestamps: `installed_at` keeps an existing value (a
    /// reinstall or a rewrite doesn't change when the game first landed),
    /// `updated_at` always becomes now.
    fn stamp(&mut self, previous: Option<&GameInstallInfo>) {
        let now = Utc::now().to_rfc3339();
        if self.installed_at.is_none() {
            self.installed_at = previous
                .and_then(|p| p.installed_at.clone())
                .or_else(|| Some(now.clone()));
        }
        self.updated_at = Some(now);
    }

    /// Persist as `<data dir>/gameinfo/<slug>.json`.
    pub fn save(&self, slug: &str) -> Result<(), GameInfoError> {
        self.save_in(&gameinfo_dir()?, slug)
    }

    /// Like [`save`](Self::save), but a failure is logged instead of
    /// returned — for callers where the install itself already succeeded.
    pub fn save_to_json(&self, slug: &str) {
        if let Err(err) = self.save(slug) {
            warn!("could not save the install record for `{slug}`: {err}");
        }
    }

    pub fn save_in(&self, dir: &Path, slug: &str) -> Result<(), GameInfoError> {
        let file = record_path(dir, slug)?;
        let previous = load_from(dir, slug).ok();

        let mut record = self.clone();
        if record.slug.is_none() {
            record.slug = Some(slug.to_owned());
        }
        record.stamp(previous.as_ref());

        fs::create_dir_all(dir)?;
        let tmp = file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&record)?)?;
        fs::rename(&tmp, &file)?;
        Ok(())
    }
}

pub fn gameinfo_dir() -> Result<PathBuf, GameInfoError> {
    Ok(maxima_dir()?.join(GAMEINFO_DIR))
}

/// Slugs name files, so keep them to the characters EA slugs actually use
/// and nothing that could walk out of the directory.
fn validate_slug(slug: &str) -> Result<(), GameInfoError> {
    let ok = !slug.is_empty()
        && slug != "."
        && slug != ".."
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(GameInfoError::InvalidSlug(slug.to_owned()))
    }
}

fn record_path(dir: &Path, slug: &str) -> Result<PathBuf, GameInfoError> {
    validate_slug(slug)?;
    Ok(dir.join(format!("{slug}.json")))
}

pub fn load_from(dir: &Path, slug: &str) -> Result<GameInstallInfo, GameInfoError> {
    let file = record_path(dir, slug)?;
    let json = match fs::read_to_string(&file) {
        Ok(json) => json,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(GameInfoError::NotFound(slug.to_owned()))
        }
        Err(err) => return Err(err.into()),
    };
    let mut info: GameInstallInfo = serde_json::from_str(&json)?;
    if info.slug.is_none() {
        info.slug = Some(slug.to_owned());
    }
    Ok(info)
}

/// Upstream's loader name, kept so ports of its call sites read the same.
pub fn load_game_info_from_json(slug: &str) -> Result<GameInstallInfo, GameInfoError> {
    load_from(&gameinfo_dir()?, slug)
}

/// `None` when there is no (readable) record.
pub fn load_game_info(slug: &str) -> Option<GameInstallInfo> {
    load_game_info_from_json(slug).ok()
}

pub fn remove_game_info(slug: &str) -> Result<(), GameInfoError> {
    let file = record_path(&gameinfo_dir()?, slug)?;
    match fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn list_in(dir: &Path) -> Vec<(String, GameInstallInfo)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, GameInstallInfo)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let slug = path.file_stem()?.to_str()?.to_owned();
            load_from(dir, &slug).ok().map(|info| (slug, info))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn list_game_infos() -> Vec<(String, GameInstallInfo)> {
    gameinfo_dir().map(|d| list_in(&d)).unwrap_or_default()
}

pub fn find_slug_by_path_in(dir: &Path, install_path: &Path) -> Option<String> {
    let wanted = normalized(install_path);
    list_in(dir)
        .into_iter()
        .find(|(_, info)| normalized(&info.path) == wanted)
        .map(|(slug, _)| slug)
}

/// Which game, if any, is recorded as installed at `install_path`.
pub fn find_slug_by_path(install_path: &Path) -> Option<String> {
    gameinfo_dir()
        .ok()
        .and_then(|d| find_slug_by_path_in(&d, install_path))
}

fn normalized(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    PathBuf::from(s.trim_end_matches(['/', '\\']))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn roundtrip_keeps_every_field() {
        let tmp = dir();
        let info = GameInstallInfo::new("/games/tf2".into(), Some("/bottles/Maxima-tf2".into()))
            .with_offer("Origin.OFR.50.0001456", Some("123"))
            .with_locale("en_US")
            .with_exclude(vec!["*.bik".into()]);
        info.save_in(tmp.path(), "titanfall-2").unwrap();

        let back = load_from(tmp.path(), "titanfall-2").unwrap();
        assert_eq!(back.path, PathBuf::from("/games/tf2"));
        assert_eq!(back.wine_prefix, Some(PathBuf::from("/bottles/Maxima-tf2")));
        assert_eq!(back.slug.as_deref(), Some("titanfall-2"));
        assert_eq!(back.offer_id.as_deref(), Some("Origin.OFR.50.0001456"));
        assert_eq!(back.build_id.as_deref(), Some("123"));
        assert_eq!(back.locale.as_deref(), Some("en_US"));
        assert_eq!(back.exclude, vec!["*.bik".to_string()]);
        assert!(back.installed_at.is_some());
        assert!(back.updated_at.is_some());
    }

    #[test]
    fn upstream_files_load_and_empty_prefix_is_none() {
        let tmp = dir();
        fs::write(
            tmp.path().join("battlefield-2042.json"),
            r#"{ "path": "C:\\Games\\BF", "wine_prefix": "" }"#,
        )
        .unwrap();
        let info = load_from(tmp.path(), "battlefield-2042").unwrap();
        assert_eq!(info.path, PathBuf::from("C:\\Games\\BF"));
        assert_eq!(info.wine_prefix, None);
        assert_eq!(info.slug.as_deref(), Some("battlefield-2042"));
        assert!(info.exclude.is_empty());
    }

    #[test]
    fn no_prefix_serializes_as_empty_string_for_upstream() {
        let tmp = dir();
        GameInstallInfo::new("/g".into(), None)
            .save_in(tmp.path(), "g")
            .unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("g.json")).unwrap()).unwrap();
        assert_eq!(raw["wine_prefix"], "");
        assert!(raw.get("exclude").is_none());
    }

    #[test]
    fn missing_or_null_prefix_reads_as_none() {
        let tmp = dir();
        fs::write(tmp.path().join("a.json"), r#"{ "path": "/a" }"#).unwrap();
        fs::write(tmp.path().join("b.json"), r#"{ "path": "/b", "wine_prefix": null }"#).unwrap();
        assert_eq!(load_from(tmp.path(), "a").unwrap().wine_prefix, None);
        assert_eq!(load_from(tmp.path(), "b").unwrap().wine_prefix, None);
    }

    #[test]
    fn rewrite_keeps_original_install_time() {
        let tmp = dir();
        let first = GameInstallInfo::new("/g".into(), None);
        first.save_in(tmp.path(), "g").unwrap();
        let installed_at = load_from(tmp.path(), "g").unwrap().installed_at;
        assert!(installed_at.is_some());

        GameInstallInfo::new("/g2".into(), None)
            .save_in(tmp.path(), "g")
            .unwrap();
        let again = load_from(tmp.path(), "g").unwrap();
        assert_eq!(again.installed_at, installed_at);
        assert_eq!(again.path, PathBuf::from("/g2"));
    }

    #[test]
    fn unsafe_slugs_are_rejected() {
        let tmp = dir();
        let info = GameInstallInfo::new("/g".into(), None);
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "a b"] {
            assert!(matches!(
                info.save_in(tmp.path(), bad),
                Err(GameInfoError::InvalidSlug(_))
            ));
            assert!(matches!(
                load_from(tmp.path(), bad),
                Err(GameInfoError::InvalidSlug(_))
            ));
        }
    }

    #[test]
    fn missing_record_is_not_found() {
        let tmp = dir();
        assert!(matches!(
            load_from(tmp.path(), "nope"),
            Err(GameInfoError::NotFound(_))
        ));
    }

    #[test]
    fn finds_slug_by_install_path() {
        let tmp = dir();
        GameInstallInfo::new("/games/one/".into(), None)
            .save_in(tmp.path(), "one")
            .unwrap();
        GameInstallInfo::new("/games/two".into(), None)
            .save_in(tmp.path(), "two")
            .unwrap();
        assert_eq!(
            find_slug_by_path_in(tmp.path(), Path::new("/games/one")).as_deref(),
            Some("one")
        );
        assert_eq!(
            find_slug_by_path_in(tmp.path(), Path::new("/games/two/")).as_deref(),
            Some("two")
        );
        assert_eq!(find_slug_by_path_in(tmp.path(), Path::new("/games/three")), None);
        assert_eq!(list_in(tmp.path()).len(), 2);
    }
}
