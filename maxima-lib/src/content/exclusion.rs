//! Per-game file exclusion: glob patterns for files that must not be
//! downloaded (and that verify must not count as missing).
//!
//! Patterns come from two places, merged: the per-game exclusion file
//! `<data dir>/exclude/<slug>` (one glob per line, `#` comments and blank
//! lines ignored — upstream PR 46's layout), and any patterns passed
//! programmatically (`--exclude` on the CLI).
//!
//! Matching is against the manifest entry name with `\` normalised to `/`
//! and case ignored, since Windows-authored manifests mix both. `*` crosses
//! `/`, so `*.bik` matches at any depth; a trailing `/` excludes a whole
//! directory (`Vo/` is `Vo/**`).

use std::path::PathBuf;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use log::{info, warn};

use crate::util::native::{maxima_dir, NativeError};

pub const EXCLUDE_DIR: &str = "exclude";

#[derive(Clone, Debug)]
pub struct ExclusionSet {
    set: GlobSet,
    patterns: Vec<String>,
}

impl Default for ExclusionSet {
    fn default() -> Self {
        Self::empty()
    }
}

fn normalize_pattern(pattern: &str) -> Option<String> {
    let mut p = pattern.trim().replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_owned();
    }
    let p = p.trim_start_matches('/');
    if p.is_empty() {
        return None;
    }
    Some(if p.ends_with('/') {
        format!("{p}**")
    } else {
        p.to_owned()
    })
}

fn normalize_name(name: &str) -> String {
    let n = name.replace('\\', "/");
    n.trim_start_matches("./").trim_start_matches('/').to_owned()
}

impl ExclusionSet {
    pub fn empty() -> Self {
        Self {
            set: GlobSetBuilder::new().build().expect("empty glob set"),
            patterns: Vec::new(),
        }
    }

    /// Build from raw patterns. Invalid or empty ones are logged and skipped
    /// rather than failing the whole list.
    pub fn new<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut builder = GlobSetBuilder::new();
        let mut kept = Vec::new();
        for raw in patterns {
            let raw = raw.as_ref();
            let Some(pattern) = normalize_pattern(raw) else {
                continue;
            };
            match GlobBuilder::new(&pattern)
                .case_insensitive(true)
                .literal_separator(false)
                .backslash_escape(false)
                .build()
            {
                Ok(glob) => {
                    builder.add(glob);
                    kept.push(raw.trim().to_owned());
                }
                Err(err) => warn!("ignoring invalid exclude pattern '{raw}': {err}"),
            }
        }
        match builder.build() {
            Ok(set) => Self { set, patterns: kept },
            Err(err) => {
                warn!("could not build the exclusion list: {err}");
                Self::empty()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn is_match(&self, name: &str) -> bool {
        !self.is_empty() && self.set.is_match(normalize_name(name))
    }
}

/// Parse the contents of an exclusion file.
pub fn parse_exclusion_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

pub fn exclusion_file_path(slug: &str) -> Result<PathBuf, NativeError> {
    // Same rule as the install records: the slug names a file.
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(NativeError::Stringify);
    }
    Ok(maxima_dir()?.join(EXCLUDE_DIR).join(slug))
}

/// Patterns from `<data dir>/exclude/<slug>`; empty when there is no file.
pub fn load_exclusion_file(slug: &str) -> Vec<String> {
    let Ok(path) = exclusion_file_path(slug) else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let patterns = parse_exclusion_file(&contents);
            if !patterns.is_empty() {
                info!("Loaded {} exclude pattern(s) from {}", patterns.len(), path.display());
            }
            patterns
        }
        Err(_) => Vec::new(),
    }
}

/// The effective exclusion list for a game: its exclusion file plus `extra`.
pub fn get_exclusion_list(slug: &str, extra: &[String]) -> ExclusionSet {
    let mut patterns = load_exclusion_file(slug);
    patterns.extend(extra.iter().cloned());
    ExclusionSet::new(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_matches_nothing() {
        let set = ExclusionSet::empty();
        assert!(set.is_empty());
        assert!(!set.is_match("anything.bik"));
    }

    #[test]
    fn star_crosses_directories() {
        let set = ExclusionSet::new(["*.bik"]);
        assert!(set.is_match("intro.bik"));
        assert!(set.is_match("Movies/Sub/intro.bik"));
        assert!(!set.is_match("intro.bik.txt"));
    }

    #[test]
    fn backslash_names_and_patterns_are_equivalent() {
        let set = ExclusionSet::new([r"Movies\*.bik"]);
        assert!(set.is_match("Movies/intro.bik"));
        assert!(set.is_match(r"Movies\intro.bik"));
        assert!(!set.is_match("Other/intro.bik"));
    }

    #[test]
    fn matching_ignores_case() {
        let set = ExclusionSet::new(["vo/*.wav"]);
        assert!(set.is_match("VO/line.WAV"));
    }

    #[test]
    fn trailing_slash_excludes_the_directory() {
        let set = ExclusionSet::new(["Vo/"]);
        assert!(set.is_match("Vo/a.wav"));
        assert!(set.is_match("vo/deep/er/b.wav"));
        assert!(!set.is_match("Voice/a.wav"));
    }

    #[test]
    fn leading_slash_and_dot_slash_anchor_the_same() {
        let set = ExclusionSet::new(["/Core/skip.dat", "./Core/skip2.dat"]);
        assert!(set.is_match("Core/skip.dat"));
        assert!(set.is_match("./Core/skip2.dat"));
        assert!(set.is_match("/Core/skip2.dat"));
        assert!(!set.is_match("Other/Core/skip.dat"));
    }

    #[test]
    fn question_mark_and_classes() {
        let set = ExclusionSet::new(["track?.ogg", "lang_[ef]n.pak"]);
        assert!(set.is_match("track1.ogg"));
        assert!(!set.is_match("track10.ogg"));
        assert!(set.is_match("lang_en.pak"));
        assert!(set.is_match("lang_fn.pak"));
        assert!(!set.is_match("lang_de.pak"));
    }

    #[test]
    fn invalid_patterns_are_skipped_not_fatal() {
        let set = ExclusionSet::new(["[unclosed", "*.bik", "   ", ""]);
        assert_eq!(set.patterns(), ["*.bik"]);
        assert!(set.is_match("a.bik"));
    }

    #[test]
    fn exclusion_file_ignores_comments_and_blanks() {
        let parsed = parse_exclusion_file("# videos\n*.bik\n\n  Vo/  \n#Core/*\n");
        assert_eq!(parsed, vec!["*.bik".to_string(), "Vo/".to_string()]);
    }

    #[test]
    fn exclusion_file_path_rejects_traversal() {
        assert!(exclusion_file_path("../etc").is_err());
        assert!(exclusion_file_path("a/b").is_err());
        assert!(exclusion_file_path("").is_err());
    }
}
