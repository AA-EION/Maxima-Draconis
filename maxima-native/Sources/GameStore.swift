import SwiftUI

/// Central UI state. One instance for the whole app; every mutation happens
/// on the main actor, every CLI interaction in a Task.
@MainActor
final class GameStore: ObservableObject {
    @Published var games: [Game] = []
    @Published var statuses: [String: GameStatus] = [:]
    @Published var loading = false
    @Published var firstLoad = true
    @Published var errorMessage: String?

    /// CrossOver's bottle directory. Honors the custom location from
    /// CrossOver's preferences plist, read straight from disk (same approach
    /// Draconis's PathResolver takes) with the standard location as
    /// fallback.
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

    func refresh() async {
        loading = true
        errorMessage = nil
        defer {
            loading = false
            firstLoad = false
        }
        do {
            let list = try await MaximaCLI.listGames()
            games = list
            for game in list {
                // Don't clobber transient states driven by active streams.
                switch statuses[game.slug] {
                case .installing, .running:
                    continue
                default:
                    statuses[game.slug] = derivedStatus(for: game)
                }
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Installed-detection: EA's `installed` flag (registry-based, empty in
    /// fresh native bottles until the game's touchup runs) OR the presence
    /// of the conventional per-game bottle install dir. The path mirrors
    /// `maxima-cli bottle-info`'s naming policy — display-speed shortcut
    /// only; all actions stay CLI-authoritative.
    private func derivedStatus(for game: Game) -> GameStatus {
        if game.installed { return .installed }
        let dir = Self.bottlesRoot()
            .appendingPathComponent("Maxima-\(game.slug)/drive_c/Games/\(game.slug)")
        return FileManager.default.fileExists(atPath: dir.path) ? .installed : .notInstalled
    }

    func install(_ game: Game) {
        Task { await runInstall(game) }
    }

    private func runInstall(_ game: Game) async {
        statuses[game.slug] = .installing(0)
        do {
            for try await ev in MaximaCLI.install(slug: game.slug) {
                switch ev.event {
                case "progress":
                    if let pct = ev.percent {
                        statuses[game.slug] = .installing(pct)
                    }
                case "done":
                    statuses[game.slug] = .installed
                case "error":
                    errorMessage = ev.message ?? "Install failed"
                    statuses[game.slug] = .notInstalled
                default:
                    break
                }
            }
            // Stream ended cleanly without an explicit done/error → treat a
            // still-"installing" card as finished.
            if case .installing = statuses[game.slug] ?? .unknown {
                statuses[game.slug] = .installed
            }
        } catch {
            errorMessage = error.localizedDescription
            statuses[game.slug] = .notInstalled
        }
    }

    func play(_ game: Game) {
        Task { await runLaunch(game) }
    }

    private func runLaunch(_ game: Game) async {
        statuses[game.slug] = .running
        do {
            for try await ev in MaximaCLI.launch(slug: game.slug) {
                if ev.event == "error" {
                    errorMessage = ev.message ?? "Launch failed"
                }
            }
        } catch {
            errorMessage = error.localizedDescription
        }
        statuses[game.slug] = .installed
    }
}
