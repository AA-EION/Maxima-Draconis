import Foundation

/// Client for `maxima-cli ui-backend` — ONE persistent child process holds
/// the logged-in session, LSX server and RTM presence (the same role the
/// egui UI's bridge_thread plays in-process; upstream PR #23's server/thin-
/// client architecture). Requests go down stdin as JSONL with ids; matched
/// responses resolve continuations; unmatched objects are pushed events.
///
/// The child is spawned CleanSpawn-style: new session, no inherited fds
/// beyond our pipes, App-Nap-disclaimed — the wine tree it spawns behaves
/// exactly as if launched from Terminal.
actor Backend {
    enum BackendError: LocalizedError {
        case notRunning
        case requestFailed(String)
        var errorDescription: String? {
            switch self {
            case .notRunning: return "Maxima backend is not running."
            case .requestFailed(let s): return s
            }
        }
    }

    private var pid: pid_t = -1
    private var stdinWriter: FileHandle?
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<[String: Any], Swift.Error>] = [:]

    private var eventContinuation: AsyncStream<[String: Any]>.Continuation?

    var isRunning: Bool { pid > 0 }

    /// Spawn the backend and return the pushed-event stream. The stream ends
    /// when the backend process dies.
    func start() throws -> AsyncStream<[String: Any]> {
        guard let cli = MaximaCLI.locate() else { throw MaximaCLIError.cliNotFound }

        let toChild = Pipe()
        let fromChild = Pipe()

        var env = ProcessInfo.processInfo.environment
        let wine = UserDefaults.standard.string(forKey: "wineCommand") ?? ""
        if !wine.isEmpty { env["MAXIMA_WINE_COMMAND"] = wine }

        let childPid = try CleanSpawn.spawn(
            executable: cli,
            arguments: ["ui-backend"],
            environment: env,
            stdinFD: toChild.fileHandleForReading.fileDescriptor,
            stdoutFD: fromChild.fileHandleForWriting.fileDescriptor,
            stderrFD: FileHandle.nullDevice.fileDescriptor
        )
        pid = childPid
        stdinWriter = toChild.fileHandleForWriting
        // Close our copies of the child-side ends so EOF propagates.
        try? toChild.fileHandleForReading.close()
        try? fromChild.fileHandleForWriting.close()

        let (stream, continuation) = AsyncStream.makeStream(of: [String: Any].self)
        eventContinuation = continuation

        let reader = fromChild.fileHandleForReading
        Task.detached { [weak self] in
            do {
                for try await line in reader.bytes.lines {
                    guard let data = line.data(using: .utf8),
                          let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
                    else { continue }
                    await self?.route(obj)
                }
            } catch {}
            // Child died / pipe closed.
            var status: Int32 = 0
            waitpid(childPid, &status, 0)
            await self?.handleExit()
        }

        return stream
    }

    private func route(_ obj: [String: Any]) {
        if let id = (obj["id"] as? NSNumber)?.uint64Value,
           let continuation = pending.removeValue(forKey: id) {
            if (obj["ok"] as? Bool) == true {
                continuation.resume(returning: obj)
            } else {
                let message = obj["error"] as? String ?? "request failed"
                continuation.resume(throwing: BackendError.requestFailed(message))
            }
        } else {
            eventContinuation?.yield(obj)
        }
    }

    private func handleExit() {
        pid = -1
        stdinWriter = nil
        for (_, cont) in pending {
            cont.resume(throwing: BackendError.notRunning)
        }
        pending.removeAll()
        eventContinuation?.finish()
        eventContinuation = nil
    }

    func stop() {
        // Closing stdin is the shutdown signal (backend exits on EOF).
        try? stdinWriter?.close()
        stdinWriter = nil
    }

    func request(_ body: [String: Any]) async throws -> [String: Any] {
        guard let writer = stdinWriter else { throw BackendError.notRunning }
        let id = nextId
        nextId += 1
        var payload = body
        payload["id"] = id
        let data = try JSONSerialization.data(withJSONObject: payload)
        return try await withCheckedThrowingContinuation { cont in
            pending[id] = cont
            do {
                try writer.write(contentsOf: data + Data("\n".utf8))
            } catch {
                pending.removeValue(forKey: id)
                cont.resume(throwing: error)
            }
        }
    }

    // Typed helpers ------------------------------------------------------

    func listGames() async throws -> [Game] {
        let resp = try await request(["cmd": "list-games"])
        let raw = resp["games"] ?? []
        let data = try JSONSerialization.data(withJSONObject: raw)
        return try JSONDecoder().decode([Game].self, from: data)
    }

    func friends() async throws -> [Friend] {
        let resp = try await request(["cmd": "friends"])
        let raw = resp["friends"] ?? []
        let data = try JSONSerialization.data(withJSONObject: raw)
        return try JSONDecoder().decode([Friend].self, from: data)
    }

    func launch(slug: String, args: [String], exeOverride: String?, cloudSaves: Bool) async throws {
        var body: [String: Any] = ["cmd": "launch", "slug": slug, "cloud_saves": cloudSaves]
        if !args.isEmpty { body["args"] = args }
        if let exe = exeOverride, !exe.isEmpty { body["exe_override"] = exe }
        _ = try await request(body)
    }

    func install(slug: String, path: String?) async throws {
        var body: [String: Any] = ["cmd": "install", "slug": slug]
        if let path, !path.isEmpty { body["path"] = path }
        _ = try await request(body)
    }
}
