//! Helpers that make `installerdata.xml` parsing tolerant of the many ways
//! EA's tooling has spelled the same thing over the years.

use serde::{Deserialize, Deserializer};

/// Parses the flag spellings seen in the wild. Unknown/empty values are
/// `false`, matching how EA's own tools treat a missing flag.
pub fn parse_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "y" | "on" | "enabled"
    )
}

/// `deserialize_with` for flags that are sometimes `true`/`false` and
/// sometimes `1`/`0`, `True`, `YES`, empty, ...
pub fn lenient_bool<'de, D: Deserializer<'de>>(de: D) -> Result<bool, D::Error> {
    Ok(parse_flag(&String::deserialize(de)?))
}

/// Same as [`lenient_bool`] for flags where "absent" and "false" differ.
pub fn lenient_opt_bool<'de, D: Deserializer<'de>>(de: D) -> Result<Option<bool>, D::Error> {
    Ok(Option::<String>::deserialize(de)?.map(|v| parse_flag(&v)))
}

/// `<name locale="en_US">Foo</name>`, `<gameTitle locale="de_DE">Bar</gameTitle>`
/// or a plain `<name>Foo</name>` without a locale.
#[derive(Default, Debug, Clone, Deserialize, PartialEq)]
pub struct LocalizedText {
    #[serde(rename = "@locale", default)]
    pub locale: String,
    #[serde(rename = "$text", default)]
    pub value: String,
}

/// Picks `preferred` if present, then `en_US`, then the first entry.
pub fn pick_localized<'a>(items: &'a [LocalizedText], preferred: &str) -> Option<&'a str> {
    items
        .iter()
        .find(|t| t.locale == preferred)
        .or_else(|| items.iter().find(|t| t.locale == "en_US"))
        .or_else(|| items.first())
        .map(|t| t.value.as_str())
}

/// Splits an argument string on whitespace, keeping quoted sections together
/// and stripping the quotes themselves (`-p "C:\My Games" -v` becomes
/// `-p`, `C:\My Games`, `-v`). An unterminated quote runs to the end of the
/// string instead of failing, since the manifest is not ours to fix.
pub fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quotes = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }

    if has_token {
        args.push(current);
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        for t in ["true", "True", "TRUE", "1", " yes "] {
            assert!(parse_flag(t), "{t}");
        }
        for f in ["false", "False", "0", "", "no", "garbage"] {
            assert!(!parse_flag(f), "{f}");
        }
    }

    #[test]
    fn split_plain() {
        assert_eq!(split_args("-a -b  -c"), ["-a", "-b", "-c"]);
        assert!(split_args("").is_empty());
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn split_quoted() {
        assert_eq!(
            split_args(r#"-install "C:\Program Files\EA Games\Foo" -silent"#),
            ["-install", r"C:\Program Files\EA Games\Foo", "-silent"]
        );
        assert_eq!(
            split_args(r#"/D="{installLocation}" /x"#),
            ["/D={installLocation}", "/x"]
        );
    }

    #[test]
    fn split_keeps_empty_quoted_arg() {
        assert_eq!(split_args(r#"-a "" -b"#), ["-a", "", "-b"]);
    }

    #[test]
    fn split_unterminated_quote() {
        assert_eq!(split_args(r#"-a "b c"#), ["-a", "b c"]);
    }

    #[test]
    fn localized_pick() {
        let items = vec![
            LocalizedText {
                locale: "de_DE".into(),
                value: "de".into(),
            },
            LocalizedText {
                locale: "en_US".into(),
                value: "en".into(),
            },
        ];
        assert_eq!(pick_localized(&items, "de_DE"), Some("de"));
        assert_eq!(pick_localized(&items, "fr_FR"), Some("en"));
        assert_eq!(pick_localized(&items[..1], "fr_FR"), Some("de"));
        assert_eq!(pick_localized(&[], "en_US"), None);
    }
}
