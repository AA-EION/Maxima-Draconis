//! Wine prefix selection, per request.
//!
//! A game's Wine prefix is decided here, for one game at a time, and handed
//! down as a plain `&Path` to everything that touches Wine (wine commands,
//! registry parsing, the license directory, cloud saves, touchup). Nothing
//! in the library mutates `MAXIMA_WINE_PREFIX` any more, so a server can
//! install game A into one prefix while launching game B from another
//! without the two seeing each other's selection.
//!
//! Precedence for [`peek_for_game`] / [`resolve_for_game`]:
//!
//! 1. an explicit prefix passed with the request (`--wine-prefix`, the proto
//!    `wine_prefix` field);
//! 2. the user override: [`set_process_override`] or the `MAXIMA_WINE_PREFIX`
//!    environment variable (read, never written, by Maxima);
//! 3. the prefix recorded when the game was installed
//!    (`gameinfo/<slug>.json`);
//! 4. the platform default for that one game: a `Maxima-<slug>` CrossOver
//!    bottle on macOS, `<data dir>/wine/prefixes/<slug>` (a private
//!    umu/Proton prefix) elsewhere.
//!
//! Operations with no game context (the `serve` passthrough, a bare registry
//! read) use [`ambient`]: the user override, else the legacy shared
//! `<data dir>/wine/prefix`.

use std::{
    path::{Path, PathBuf},
    sync::RwLock,
};

use crate::{
    gameinfo,
    util::native::{maxima_dir, NativeError},
};

pub const WINE_PREFIX_ENV: &str = "MAXIMA_WINE_PREFIX";

/// An override installed by the embedding binary (e.g. `maxima-cli serve
/// --wine-prefix`). Same weight as the environment variable, without having
/// to mutate the process environment.
static PROCESS_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

pub fn set_process_override(path: Option<PathBuf>) {
    if let Ok(mut guard) = PROCESS_OVERRIDE.write() {
        *guard = path.filter(|p| !p.as_os_str().is_empty());
    }
}

/// The user's explicit prefix choice, if any: the process override, else
/// `MAXIMA_WINE_PREFIX`.
pub fn explicit_override() -> Option<PathBuf> {
    if let Ok(guard) = PROCESS_OVERRIDE.read() {
        if let Some(path) = guard.as_ref() {
            return Some(path.clone());
        }
    }
    std::env::var_os(WINE_PREFIX_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The single shared prefix older versions used for every game.
pub fn legacy_shared_prefix() -> Result<PathBuf, NativeError> {
    Ok(maxima_dir()?.join("wine/prefix"))
}

/// Prefix for operations with no game context.
pub fn ambient() -> Result<PathBuf, NativeError> {
    match explicit_override() {
        Some(path) => Ok(path),
        None => legacy_shared_prefix(),
    }
}

fn check_slug(slug: &str) -> Result<(), NativeError> {
    let ok = !slug.is_empty()
        && slug != "."
        && slug != ".."
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(NativeError::InvalidSlug(slug.to_owned()))
    }
}

/// Where a game's prefix lives when nothing else chose one. Creates nothing.
pub fn default_prefix_for(slug: &str) -> Result<PathBuf, NativeError> {
    check_slug(slug)?;
    if cfg!(target_os = "macos") {
        super::crossover::game_bottle_path(slug)
    } else {
        Ok(maxima_dir()?.join("wine/prefixes").join(slug))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixSource {
    /// Passed with the request.
    Explicit,
    /// `MAXIMA_WINE_PREFIX` / [`set_process_override`].
    Override,
    /// Recorded in the game's install record.
    Recorded,
    /// The platform's per-game default.
    Default,
}

/// The precedence rules, free of any I/O so they can be tested.
pub fn select(
    explicit: Option<&Path>,
    user_override: Option<PathBuf>,
    recorded: Option<PathBuf>,
    default: impl FnOnce() -> Result<PathBuf, NativeError>,
) -> Result<(PathBuf, PrefixSource), NativeError> {
    if let Some(path) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Ok((path.to_path_buf(), PrefixSource::Explicit));
    }
    if let Some(path) = user_override {
        return Ok((path, PrefixSource::Override));
    }
    if let Some(path) = recorded {
        return Ok((path, PrefixSource::Recorded));
    }
    Ok((default()?, PrefixSource::Default))
}

/// The prefix a game would use, and why. Creates nothing.
pub fn peek_for_game(
    slug: &str,
    explicit: Option<&Path>,
) -> Result<(PathBuf, PrefixSource), NativeError> {
    check_slug(slug)?;
    select(
        explicit,
        explicit_override(),
        recorded_prefix(slug),
        || default_prefix_for(slug),
    )
}

/// The prefix recorded for `slug` when it was installed.
pub fn recorded_prefix(slug: &str) -> Option<PathBuf> {
    gameinfo::load_game_info(slug).and_then(|info| info.wine_prefix)
}

/// Like [`peek_for_game`], without the provenance.
pub fn prefix_for_game(slug: &str, explicit: Option<&Path>) -> Result<PathBuf, NativeError> {
    peek_for_game(slug, explicit).map(|(path, _)| path)
}

/// The prefix a game runs in, created if it is one of ours to create: the
/// per-game CrossOver bottle on macOS (via `cxbottle`), the private
/// `wine/prefixes/<slug>` directory on Linux. A prefix the user named
/// explicitly is returned as is on macOS (it must already be a bottle) and
/// created as a directory elsewhere (umu initialises it on first run).
pub async fn resolve_for_game(
    slug: &str,
    explicit: Option<&Path>,
) -> Result<PathBuf, NativeError> {
    let (path, source) = peek_for_game(slug, explicit)?;
    ensure_exists(slug, &path, source).await?;
    Ok(path)
}

#[cfg(target_os = "macos")]
async fn ensure_exists(slug: &str, path: &Path, source: PrefixSource) -> Result<(), NativeError> {
    if path.join("system.reg").exists() {
        return Ok(());
    }
    // Only the bottle Maxima itself names is Maxima's to create.
    if matches!(source, PrefixSource::Default | PrefixSource::Recorded)
        && default_prefix_for(slug).ok().as_deref() == Some(path)
    {
        super::crossover::ensure_game_bottle(slug).await?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn ensure_exists(_slug: &str, path: &Path, _source: PrefixSource) -> Result<(), NativeError> {
    if !path.exists() {
        tokio::fs::create_dir_all(path).await?;
    }
    Ok(())
}

/// CrossOver bottles are selected by name, derived from the prefix directory.
pub fn bottle_name(prefix: &Path) -> Option<String> {
    prefix
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// The profile name Wine creates for the user in a fresh prefix.
fn default_profile_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "crossover"
    } else {
        "steamuser"
    }
}

/// Pick the Wine user profile directory out of `<prefix>/drive_c/users`.
///
/// The profile's name depends on the host: plain Wine uses `$USER`,
/// CrossOver `crossover`, Proton/umu `steamuser`. Prefer those (in that
/// order); otherwise take the only real profile, or the alphabetically first
/// when there are several. `Public` and the stock `All Users`/`Default`
/// entries never count.
pub fn pick_user_dir(users_dir: &Path, host_user: Option<&str>) -> Option<PathBuf> {
    const STOCK: [&str; 4] = ["public", "all users", "default", "default user"];

    let mut names: Vec<String> = std::fs::read_dir(users_dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !STOCK.contains(&name.to_lowercase().as_str()))
        .collect();
    names.sort();

    let preferred = host_user
        .filter(|u| !u.is_empty())
        .into_iter()
        .chain(["crossover", "steamuser"])
        .find_map(|want| names.iter().find(|n| n.eq_ignore_ascii_case(want)));

    preferred
        .or_else(|| names.first())
        .map(|name| users_dir.join(name))
}

fn host_user() -> Option<String> {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
}

/// The Wine user's home (`C:\users\<name>`) inside `prefix`, as a host path.
/// Falls back to the profile Wine would create there when the prefix has not
/// run yet.
pub fn wine_user_dir(prefix: &Path) -> PathBuf {
    let users = prefix.join("drive_c").join("users");
    pick_user_dir(&users, host_user().as_deref()).unwrap_or_else(|| users.join(default_profile_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_ok() -> Result<PathBuf, NativeError> {
        Ok(PathBuf::from("/default/Maxima-g"))
    }

    #[test]
    fn explicit_beats_everything() {
        let (path, source) = select(
            Some(Path::new("/explicit")),
            Some("/override".into()),
            Some("/recorded".into()),
            default_ok,
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/explicit"));
        assert_eq!(source, PrefixSource::Explicit);
    }

    #[test]
    fn override_beats_record_and_default() {
        let (path, source) =
            select(None, Some("/override".into()), Some("/recorded".into()), default_ok).unwrap();
        assert_eq!(path, PathBuf::from("/override"));
        assert_eq!(source, PrefixSource::Override);
    }

    #[test]
    fn record_beats_default() {
        let (path, source) = select(None, None, Some("/recorded".into()), default_ok).unwrap();
        assert_eq!(path, PathBuf::from("/recorded"));
        assert_eq!(source, PrefixSource::Recorded);
    }

    #[test]
    fn falls_back_to_the_per_game_default() {
        let (path, source) = select(None, None, None, default_ok).unwrap();
        assert_eq!(path, PathBuf::from("/default/Maxima-g"));
        assert_eq!(source, PrefixSource::Default);
    }

    #[test]
    fn empty_explicit_is_ignored() {
        let (_, source) = select(Some(Path::new("")), None, None, default_ok).unwrap();
        assert_eq!(source, PrefixSource::Default);
    }

    #[test]
    fn default_is_not_evaluated_when_something_else_chose() {
        let result = select(Some(Path::new("/x")), None, None, || {
            panic!("default must stay lazy")
        });
        assert!(result.is_ok());
    }

    #[test]
    fn two_games_resolve_to_different_defaults() {
        let a = default_prefix_for("game-a").unwrap();
        let b = default_prefix_for("game-b").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("game-a") || a.ends_with("Maxima-game-a"));
        assert!(b.ends_with("game-b") || b.ends_with("Maxima-game-b"));
    }

    #[test]
    fn slugs_cannot_escape_the_prefix_dir() {
        for bad in ["", "..", "../x", "a/b", "a\\b"] {
            assert!(matches!(
                default_prefix_for(bad),
                Err(NativeError::InvalidSlug(_))
            ));
        }
    }

    fn users_with(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for name in names {
            std::fs::create_dir_all(tmp.path().join(name)).unwrap();
        }
        tmp
    }

    #[test]
    fn user_dir_prefers_host_then_crossover_then_steamuser() {
        let tmp = users_with(&["Public", "steamuser", "crossover", "alice"]);
        assert_eq!(
            pick_user_dir(tmp.path(), Some("alice")),
            Some(tmp.path().join("alice"))
        );
        assert_eq!(
            pick_user_dir(tmp.path(), Some("bob")),
            Some(tmp.path().join("crossover"))
        );
        let tmp = users_with(&["Public", "steamuser"]);
        assert_eq!(pick_user_dir(tmp.path(), None), Some(tmp.path().join("steamuser")));
    }

    #[test]
    fn user_dir_ignores_stock_profiles_and_takes_the_only_real_one() {
        let tmp = users_with(&["Public", "All Users", "Default", "Default User", "someone"]);
        assert_eq!(pick_user_dir(tmp.path(), Some("else")), Some(tmp.path().join("someone")));
    }

    #[test]
    fn user_dir_none_when_only_stock_profiles() {
        let tmp = users_with(&["Public"]);
        assert_eq!(pick_user_dir(tmp.path(), Some("x")), None);
        assert_eq!(pick_user_dir(&tmp.path().join("missing"), None), None);
    }

    #[test]
    fn user_dir_host_match_ignores_case() {
        let tmp = users_with(&["Alice", "crossover"]);
        assert_eq!(
            pick_user_dir(tmp.path(), Some("alice")),
            Some(tmp.path().join("Alice"))
        );
    }

    #[test]
    fn wine_user_dir_falls_back_for_a_fresh_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = wine_user_dir(tmp.path());
        assert!(dir.starts_with(tmp.path().join("drive_c/users")));
        assert_eq!(
            dir.file_name().unwrap().to_str().unwrap(),
            default_profile_name()
        );
    }

    #[test]
    fn wine_user_dir_reads_the_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["Public", "crossover"] {
            std::fs::create_dir_all(tmp.path().join("drive_c/users").join(name)).unwrap();
        }
        // Whatever $USER is on the machine running the test, `crossover` is
        // the only real profile in this prefix.
        let dir = wine_user_dir(tmp.path());
        assert_eq!(dir.file_name().unwrap(), "crossover");
    }
}
