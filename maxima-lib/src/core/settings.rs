use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MaximaSetting {
    #[default]
    IsIgoEnabled,
    IsIgoAvailable,
    Environment,
    /// The game's language, as an `xx_XX` locale.
    Language,
}
