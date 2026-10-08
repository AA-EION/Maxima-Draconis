//! Wire DTOs — the data that crosses between the Maxima server and its
//! frontends. Plain serde structs (no maxima-lib types) so the client side
//! links none of the server's logic. The server maps its rich internal types
//! onto these; frontends map these onto their UI types.

use serde::{Deserialize, Serialize};

/// One owned title (mirrors the server's `list-games --json` object).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GameDto {
    pub slug: String,
    pub name: String,
    pub offer_id: String,
    pub content_id: String,
    pub display_name: String,
    pub installed: bool,
    pub install_path: Option<String>,
    pub version: Option<String>,
    pub has_cloud_save: bool,
    #[serde(default)]
    pub extra_offers: Vec<ExtraOfferDto>,
    /// Box-art / hero image URLs — so a UI's image loader can fetch them
    /// without touching the service layer. `default` keeps older peers
    /// deserializing.
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub hero_url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExtraOfferDto {
    pub offer_id: String,
    pub display_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FriendDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// The signed-in user (persona + id + avatar), for a thin client that needs
/// more than the persona string the `ready` notification carries.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct UserDto {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// Per-game image URLs, fetched lazily (the server runs the service-layer
/// image requests; the UI feeds these URLs to its own image loader).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GameImagesDto {
    pub hero: Option<String>,
    pub logo: Option<String>,
    pub background: Option<String>,
}

/// Rich per-game detail (mirrors the egui UI's `GameDetails`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GameDetailsDto {
    /// Hours played × 10 (one-decimal precision).
    pub time: u32,
    pub achievements_unlocked: u16,
    pub achievements_total: u16,
    pub path: String,
    pub system_requirements_min: Option<String>,
    pub system_requirements_rec: Option<String>,
}

/// Live presence for one friend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PresenceDto {
    pub id: String,
    pub basic: String,
    pub status: String,
    pub game: Option<String>,
}

impl PresenceDto {
    pub fn is_online(&self) -> bool {
        self.basic != "Offline" && self.basic != "Unknown"
    }
}

/// Read-only bottle / wine-prefix / game-dir readout for a title.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BottleInfoDto {
    pub slug: String,
    pub bottle_name: Option<String>,
    pub wine_prefix: Option<String>,
    pub wine_prefix_exists: bool,
    pub default_game_dir: Option<String>,
    pub game_dir_exists: bool,
}

/// Result of a verify pass over a game's files.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct VerifyResultDto {
    pub verified: u64,
    pub broken: Vec<String>,
    pub repaired: bool,
}

/// Session status snapshot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StatusDto {
    pub persona: String,
    pub playing: bool,
    pub installing: Option<String>,
    pub lsx_port: u16,
    pub clients: u64,
}
