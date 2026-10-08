import Foundation

/// One owned title from `maxima-cli list-games --json`.
struct Game: Decodable, Identifiable, Hashable {
    let slug: String
    let name: String
    let offerId: String
    let contentId: String
    let displayName: String
    let installed: Bool
    let installPath: String?
    let version: String?
    let hasCloudSave: Bool

    var id: String { slug }

    enum CodingKeys: String, CodingKey {
        case slug, name, installed, version
        case offerId = "offer_id"
        case contentId = "content_id"
        case displayName = "display_name"
        case installPath = "install_path"
        case hasCloudSave = "has_cloud_save"
    }
}

/// Output of `maxima-cli bottle-info <slug> --json`.
struct BottleInfo: Decodable {
    let slug: String
    let bottleName: String?
    let winePrefix: String?
    let winePrefixExists: Bool
    let defaultGameDir: String?
    let gameDirExists: Bool

    enum CodingKeys: String, CodingKey {
        case slug
        case bottleName = "bottle_name"
        case winePrefix = "wine_prefix"
        case winePrefixExists = "wine_prefix_exists"
        case defaultGameDir = "default_game_dir"
        case gameDirExists = "game_dir_exists"
    }
}

/// One JSONL event from `install --json` / `launch --json`. Fields are a
/// union across event types; absent ones decode to nil.
struct CLIEvent: Decodable {
    let event: String
    let percent: Double?
    let message: String?
    let elapsedSecs: Double?
    let winePrefix: String?

    enum CodingKeys: String, CodingKey {
        case event, percent, message
        case elapsedSecs = "elapsed_secs"
        case winePrefix = "wine_prefix"
    }
}

enum GameStatus: Equatable {
    case unknown
    case notInstalled
    case installed
    case installing(Double) // percent, 0–100
    case running
}

/// A friend from the backend's `friends` response.
struct Friend: Decodable, Identifiable, Hashable {
    let id: String
    let name: String
}

/// Live presence for one friend, updated by pushed `presence` events.
struct Presence: Equatable {
    let basic: String
    let status: String
    let game: String?

    var isOnline: Bool { basic != "Offline" && basic != "Unknown" }
}

/// Per-game launch preferences, persisted locally (feature parity with the
/// egui UI's per-game settings modal).
struct GameLocalSettings: Codable, Equatable {
    var launchArgs: String = ""
    var exeOverride: String = ""
    var cloudSaves: Bool = true
}

enum BackendState: Equatable {
    case connecting
    case ready(persona: String)
    case stopped(reason: String?)
}
