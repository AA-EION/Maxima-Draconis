import Foundation

/// Bridge to the native `maxima-cli` binary. All game state and actions go
/// through the CLI's machine-readable surface (`list-games` / `install` /
/// `launch` `--json`, `bottle-info`, `register-protocols`) — the exact
/// contract Draconis consumes. This app is deliberately just another
/// consumer of that contract; nothing here talks to EA directly.
enum MaximaCLIError: LocalizedError {
    case cliNotFound
    case commandFailed(String)

    var errorDescription: String? {
        switch self {
        case .cliNotFound:
            return "maxima-cli not found. Build it with `cargo build --release -p maxima-cli` or set its path in Settings."
        case .commandFailed(let s):
            return s
        }
    }
}

enum MaximaCLI {
    /// Resolution order: user setting → env → bundled (Contents/Resources)
    /// → dev tree (repo target/release relative to the app bundle) → /usr/local/bin.
    static func locate() -> URL? {
        let fm = FileManager.default

        let custom = UserDefaults.standard.string(forKey: "maximaCliPath") ?? ""
        if !custom.isEmpty, fm.isExecutableFile(atPath: custom) {
            return URL(fileURLWithPath: custom)
        }
        if let env = ProcessInfo.processInfo.environment["MAXIMA_CLI_PATH"],
           fm.isExecutableFile(atPath: env) {
            return URL(fileURLWithPath: env)
        }
        if let bundled = Bundle.main.url(forResource: "maxima-cli", withExtension: nil),
           fm.isExecutableFile(atPath: bundled.path) {
            return bundled
        }
        // Dev layout: maxima-native/build/Maxima.app → <repo>/target/release
        let dev = Bundle.main.bundleURL
            .deletingLastPathComponent() // build/
            .deletingLastPathComponent() // maxima-native/
            .deletingLastPathComponent() // repo root
            .appendingPathComponent("target/release/maxima-cli")
        if fm.isExecutableFile(atPath: dev.path) {
            return dev
        }
        let usrLocal = "/usr/local/bin/maxima-cli"
        if fm.isExecutableFile(atPath: usrLocal) {
            return URL(fileURLWithPath: usrLocal)
        }
        return nil
    }

    private static func makeProcess(arguments: [String]) throws -> Process {
        guard let cli = locate() else { throw MaximaCLIError.cliNotFound }
        let p = Process()
        p.executableURL = cli
        p.arguments = arguments
        var env = ProcessInfo.processInfo.environment
        // Wine engine selection from Settings — same knob the CLI and the
        // egui UI honor. Empty = auto (CrossOver on macOS).
        let wine = UserDefaults.standard.string(forKey: "wineCommand") ?? ""
        if !wine.isEmpty {
            env["MAXIMA_WINE_COMMAND"] = wine
        }
        p.environment = env
        return p
    }

    /// Run a subcommand to completion; returns stdout. Throws with stderr on
    /// non-zero exit.
    static func run(_ arguments: [String]) async throws -> String {
        let p = try makeProcess(arguments: arguments)
        let out = Pipe()
        let err = Pipe()
        p.standardOutput = out
        p.standardError = err
        try p.run()

        // EOF arrives when the process exits; read off the cooperative pool.
        let stdoutData = try await Task.detached {
            try out.fileHandleForReading.readToEnd() ?? Data()
        }.value
        let stderrData = try await Task.detached {
            try err.fileHandleForReading.readToEnd() ?? Data()
        }.value
        p.waitUntilExit() // immediate: both pipes already hit EOF

        guard p.terminationStatus == 0 else {
            let msg = String(data: stderrData, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            throw MaximaCLIError.commandFailed(
                msg.isEmpty ? "maxima-cli exited with \(p.terminationStatus)" : msg)
        }
        return String(data: stdoutData, encoding: .utf8) ?? ""
    }

    /// Spawn a JSONL-emitting subcommand; yields one `CLIEvent` per stdout
    /// line. The stream finishes when the process exits (throwing on
    /// non-zero); cancelling the consuming task terminates the process.
    static func stream(_ arguments: [String]) -> AsyncThrowingStream<CLIEvent, Error> {
        AsyncThrowingStream { continuation in
            let p: Process
            do {
                p = try makeProcess(arguments: arguments)
            } catch {
                continuation.finish(throwing: error)
                return
            }
            let out = Pipe()
            p.standardOutput = out
            p.standardError = FileHandle.nullDevice

            do {
                try p.run()
            } catch {
                continuation.finish(throwing: error)
                return
            }

            let handle = out.fileHandleForReading
            Task.detached {
                do {
                    for try await line in handle.bytes.lines {
                        guard let data = line.data(using: .utf8),
                              let ev = try? JSONDecoder().decode(CLIEvent.self, from: data)
                        else { continue }
                        continuation.yield(ev)
                    }
                } catch {
                    // Pipe read error — fall through to exit-status handling.
                }
                p.waitUntilExit()
                if p.terminationStatus == 0 {
                    continuation.finish()
                } else {
                    continuation.finish(throwing: MaximaCLIError.commandFailed(
                        "maxima-cli exited with \(p.terminationStatus)"))
                }
            }
            continuation.onTermination = { _ in
                if p.isRunning { p.terminate() }
            }
        }
    }

    // High-level API ------------------------------------------------------

    static func listGames() async throws -> [Game] {
        let out = try await run(["list-games", "--json"])
        guard let data = out.data(using: .utf8) else { return [] }
        return try JSONDecoder().decode([Game].self, from: data)
    }

    static func bottleInfo(slug: String) async throws -> BottleInfo {
        let out = try await run(["bottle-info", slug, "--json"])
        guard let data = out.data(using: .utf8) else {
            throw MaximaCLIError.commandFailed("empty bottle-info output")
        }
        return try JSONDecoder().decode(BottleInfo.self, from: data)
    }

    static func registerProtocols() async throws {
        _ = try await run(["register-protocols"])
    }

    static func install(slug: String) -> AsyncThrowingStream<CLIEvent, Error> {
        stream(["install", slug, "--json"])
    }

    static func launch(slug: String) -> AsyncThrowingStream<CLIEvent, Error> {
        stream(["launch", slug, "--json"])
    }
}
