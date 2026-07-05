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

/// Session status snapshot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StatusDto {
    pub persona: String,
    pub playing: bool,
    pub installing: Option<String>,
    pub lsx_port: u16,
    pub clients: u64,
}
