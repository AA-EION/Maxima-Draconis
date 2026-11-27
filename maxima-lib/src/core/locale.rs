//! Game locales, as the `xx_XX` codes EA uses everywhere (`en_US`, `de_DE`,
//! `pt_BR`, …).
//!
//! A launch or install picks its locale from, in order: the request itself,
//! the game's install record, then the process default — `MAXIMA_LOCALE`, or
//! `en_US` when that is unset. Nothing here knows about any one title: the
//! code is only checked for shape, never against a per-game list.

use std::{env, fmt, str::FromStr};

use log::warn;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub const DEFAULT_LOCALE: &str = "en_US";
pub const LOCALE_ENV: &str = "MAXIMA_LOCALE";

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("`{0}` is not a locale (expected a code like en_US)")]
pub struct LocaleError(pub String);

/// A validated, normalized `xx_XX` locale code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Locale(String);

impl Locale {
    /// Accepts `en_US`, and the `en-US` / `en_us` spellings, normalized to
    /// `en_US`. Anything else is rejected.
    pub fn parse(value: &str) -> Result<Self, LocaleError> {
        let invalid = || LocaleError(value.to_owned());
        let (language, region) = value.trim().split_once(['_', '-']).ok_or_else(invalid)?;
        let two_letters = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic());
        if !two_letters(language) || !two_letters(region) {
            return Err(invalid());
        }
        Ok(Self(format!(
            "{}_{}",
            language.to_ascii_lowercase(),
            region.to_ascii_uppercase()
        )))
    }

    pub fn en_us() -> Self {
        Self(DEFAULT_LOCALE.to_owned())
    }

    /// `MAXIMA_LOCALE`, if set to a valid code. An invalid value is ignored
    /// with a warning.
    pub fn from_env() -> Option<Self> {
        let value = env::var(LOCALE_ENV).ok().filter(|v| !v.trim().is_empty())?;
        match Self::parse(&value) {
            Ok(locale) => Some(locale),
            Err(err) => {
                warn!("Ignoring {LOCALE_ENV}: {err}");
                None
            }
        }
    }

    /// The process default: `MAXIMA_LOCALE`, else `en_US`.
    pub fn process_default() -> Self {
        Self::from_env().unwrap_or_default()
    }

    /// First valid choice of an explicit request value (an invalid one is an
    /// error: the caller asked for it), a recorded value (an invalid one is
    /// skipped) and `fallback`.
    pub fn resolve(
        explicit: Option<&str>,
        recorded: Option<&str>,
        fallback: &Locale,
    ) -> Result<Self, LocaleError> {
        if let Some(explicit) = explicit {
            return Self::parse(explicit);
        }
        Ok(recorded
            .and_then(|r| Self::parse(r).ok())
            .unwrap_or_else(|| fallback.clone()))
    }

    /// The language part, e.g. `en`.
    pub fn short_str(&self) -> &str {
        &self.0[..2]
    }

    /// The full code, e.g. `en_US`.
    pub fn full_str(&self) -> &str {
        &self.0
    }

    /// The service layer's spelling (`EnUs`), which is what the GraphQL
    /// requests have always sent.
    pub fn service_layer_name(&self) -> String {
        let mut out = String::with_capacity(4);
        for (i, c) in self.0.chars().filter(|c| *c != '_').enumerate() {
            out.push(if i % 2 == 0 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
        }
        out
    }

    pub fn serialize_service_layer<S: Serializer>(
        locale: &Locale,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&locale.service_layer_name())
    }

    fn from_service_layer_name(value: &str) -> Option<Self> {
        let ok = value.len() == 4 && value.bytes().all(|b| b.is_ascii_alphabetic());
        ok.then(|| Self::parse(&format!("{}_{}", &value[..2], &value[2..])).ok())
            .flatten()
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self::en_us()
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Locale {
    type Err = LocaleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Locale {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Locale {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value)
            .ok()
            .or_else(|| Self::from_service_layer_name(&value))
            .ok_or_else(|| de::Error::custom(LocaleError(value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes() {
        assert_eq!(Locale::parse("en_US").unwrap().full_str(), "en_US");
        assert_eq!(Locale::parse("de-de").unwrap().full_str(), "de_DE");
        assert_eq!(Locale::parse(" PT_br ").unwrap().full_str(), "pt_BR");
        assert_eq!(Locale::parse("fr_FR").unwrap().short_str(), "fr");
    }

    #[test]
    fn rejects_malformed_codes() {
        for bad in ["", "en", "en_", "_US", "eng_US", "en_USA", "e1_US", "en US", "en_US_x", "../x"] {
            assert!(Locale::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn resolve_prefers_explicit_then_record_then_fallback() {
        let fallback = Locale::parse("ja_JP").unwrap();
        assert_eq!(
            Locale::resolve(Some("es_MX"), Some("de_DE"), &fallback).unwrap().full_str(),
            "es_MX"
        );
        assert_eq!(Locale::resolve(None, Some("de_DE"), &fallback).unwrap().full_str(), "de_DE");
        assert_eq!(Locale::resolve(None, Some("garbage"), &fallback).unwrap(), fallback);
        assert_eq!(Locale::resolve(None, None, &fallback).unwrap(), fallback);
        assert!(Locale::resolve(Some("nope"), None, &fallback).is_err());
    }

    #[test]
    fn service_layer_spelling_is_unchanged() {
        assert_eq!(Locale::en_us().service_layer_name(), "EnUs");
        assert_eq!(Locale::parse("pt_BR").unwrap().service_layer_name(), "PtBr");
    }

    #[test]
    fn serde_round_trip() {
        let locale = Locale::parse("it_IT").unwrap();
        let json = serde_json::to_string(&locale).unwrap();
        assert_eq!(json, "\"it_IT\"");
        assert_eq!(serde_json::from_str::<Locale>(&json).unwrap(), locale);
        assert_eq!(serde_json::from_str::<Locale>("\"EnUs\"").unwrap(), Locale::en_us());
        assert!(serde_json::from_str::<Locale>("\"english\"").is_err());
    }
}
