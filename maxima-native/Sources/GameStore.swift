import SwiftUI

/// Central UI state, fed by ONE persistent `maxima-cli ui-backend` process
/// (login, LSX, RTM presence held open — the same session model as the egui
/// UI). All mutations on the main actor.
@MainActor
final class GameStore: ObservableObject {
    @Published var backendState: BackendState = .connecting
    @Published var games: [Game] = []
    @Published var statuses: [String: GameStatus] = [:]
    @Published var friends: [Friend] = []
    @Published var presences: [String: Presence] = [:]
    @Published var errorMessage: String?
    @Published var loading = false

    private let backend = Backend()
    private var lastLaunched: String?

    var activeInstalls: [(game: Game, percent: Double)] {
        games.compactMap { game in
            if case .installing(let pct) = statuses[game.slug] ?? .unknown {
                return (game, pct)
            }
            return nil
        }
    }

    var onlineFriendCount: Int {
        friends.filter { presences[$0.id]?.isOnline == true }.count
    }

    // Backend lifecycle ---------------------------------------------------

    func start() {
        Task { await runBackend() }
    }

    func shutdown() {
        Task { await backend.stop() }
    }

    private func runBackend() async {
        backendState = .connecting
        do {
            let events = try await backend.start()
            Task { [weak self] in
                for await event in events {
                    await self?.handle(event: event)
                }
                await MainActor.run { [weak self] in
                    if case .stopped = self?.backendState ?? .connecting { return }
                    self?.backendState = .stopped(reason: "Maxima backend exited")
                }
            }
        } catch {
            backendState = .stopped(reason: error.localizedDescription)
        }
    }

    private func handle(event: [String: Any]) {
        switch event["event"] as? String {
        case "ready":
            backendState = .ready(persona: event["persona"] as? String ?? "")
            Task { await refreshAll() }
        case "presence":
            guard let id = event["id"] as? String else { return }
            presences[id] = Presence(
                basic: event["basic"] as? String ?? "Unknown",
                status: event["status"] as? String ?? "",
                game: event["game"] as? String
            )
        case "install-progress":
            if let slug = event["slug"] as? String {
                statuses[slug] = .installing(event["percent"] as? Double ?? 0)
            }
        case "install-done":
            if let slug = event["slug"] as? String {
                statuses[slug] = .installed
            }
        case "install-error":
            if let slug = event["slug"] as? String {
                statuses[slug] = .notInstalled
            }
            errorMessage = event["message"] as? String ?? "Install failed"
        case "game-started":
            if let slug = event["slug"] as? String {
                lastLaunched = slug
                statuses[slug] = .running
            }
        case "game-stopped":
            if let slug = lastLaunched {
                statuses[slug] = .installed
                lastLaunched = nil
            }
        case "error":
            errorMessage = event["message"] as? String
        default:
            break
        }
    }

    // Data ----------------------------------------------------------------

    func refreshAll() async {
        loading = true
        defer { loading = false }
        do {
            let list = try await backend.listGames()
            games = list
            for game in list {
                switch statuses[game.slug] {
                case .installing, .running:
                    continue
                default:
                    statuses[game.slug] = derivedStatus(for: game)
                }
            }
            friends = try await backend.friends()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Installed-detection: EA's registry-based flag OR the conventional
    /// per-game bottle dir (mirror of `bottle-info`'s naming policy —
    /// display shortcut only; actions stay backend-authoritative).
    private func derivedStatus(for game: Game) -> GameStatus {
        if game.installed { return .installed }
        let dir = Self.bottlesRoot()
            .appendingPathComponent("Maxima-\(game.slug)/drive_c/Games/\(game.slug)")
        return FileManager.default.fileExists(atPath: dir.path) ? .installed : .notInstalled
    }

    /// CrossOver's bottle directory — custom location honored from its
    /// preferences plist, read straight from disk (Draconis PathResolver
    /// approach), standard path as fallback.
    static func bottlesRoot() -> URL {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let fallback = home.appendingPathComponent(
            "Library/Application Support/CrossOver/Bottles", isDirectory: true)
        let plist = home.appendingPathComponent(
            "Library/Preferences/com.codeweavers.CrossOver.plist")
        guard let data = try? Data(contentsOf: plist),
              let dict = try? PropertyListSerialization.propertyList(
                  from: data, options: [], format: nil) as? [String: Any],
              let raw = dict["BottleDir"] as? String,
              !raw.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return fallback }
        return URL(fileURLWithPath: (raw as NSString).standardizingPath, isDirectory: true)
    }

    // Per-game settings (parity with the egui per-game settings modal) ----

    func localSettings(for slug: String) -> GameLocalSettings {
        guard let data = UserDefaults.standard.data(forKey: "gameSettings.\(slug)"),
              let settings = try? JSONDecoder().decode(GameLocalSettings.self, from: data)
        else { return GameLocalSettings() }
        return settings
    }

    func setLocalSettings(_ settings: GameLocalSettings, for slug: String) {
        if let data = try? JSONEncoder().encode(settings) {
            UserDefaults.standard.set(data, forKey: "gameSettings.\(slug)")
        }
    }

    // Actions ---------------------------------------------------------------

    func play(_ game: Game) {
        let settings = localSettings(for: game.slug)
        let args = settings.launchArgs
            .split(separator: " ")
            .map(String.init)
            .filter { !$0.isEmpty }
        statuses[game.slug] = .running
        lastLaunched = game.slug
        Task {
            do {
                try await backend.launch(
                    slug: game.slug,
                    args: args,
                    exeOverride: settings.exeOverride,
                    cloudSaves: settings.cloudSaves
                )
            } catch {
                errorMessage = error.localizedDescription
                statuses[game.slug] = derivedStatus(for: game)
                lastLaunched = nil
            }
        }
    }

    func install(_ game: Game) {
        statuses[game.slug] = .installing(0)
        Task {
            do {
                try await backend.install(slug: game.slug, path: nil)
            } catch {
                errorMessage = error.localizedDescription
                statuses[game.slug] = .notInstalled
            }
        }
    }
}
