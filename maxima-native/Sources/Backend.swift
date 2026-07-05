import Darwin
import Foundation

/// Client for the multi-client `maxima-cli server` (upstream PR #23's
/// server/thin-client architecture). Connects over loopback TCP; if no server
/// is running it spawns one (`maxima-cli server`) detached and waits for it.
/// Because the game is responsibility-disclaimed at the cxstart hop inside the
/// server, the server can be started any way — by this app, at logon, or by
/// another frontend — and every client shares one synced session.
///
/// Requests go down as JSONL with ids; matched responses resolve
/// continuations, unmatched objects are pushed events.
actor Backend {
    enum BackendError: LocalizedError {
        case notConnected
        case cliNotFound
        case serverUnavailable
        case requestFailed(String)
        var errorDescription: String? {
            switch self {
            case .notConnected: return "Not connected to the Maxima server."
            case .cliNotFound: return "maxima-cli not found (set its path in Settings)."
            case .serverUnavailable: return "The Maxima server did not start."
            case .requestFailed(let s): return s
            }
        }
    }

    static let port: UInt16 = {
        if let s = ProcessInfo.processInfo.environment["MAXIMA_SERVER_PORT"],
           let p = UInt16(s) { return p }
        return 13220
    }()

    private var writeHandle: FileHandle?
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<[String: Any], Swift.Error>] = [:]
    private var eventContinuation: AsyncStream<[String: Any]>.Continuation?

    var isConnected: Bool { writeHandle != nil }

    /// Ensure a server is up (spawn if needed), connect, and return the
    /// pushed-event stream. The stream ends if the connection drops.
    func start() async throws -> AsyncStream<[String: Any]> {
        if !Self.probe() {
            try spawnServer()
            // Wait for it to answer (login may run on first start).
            var up = false
            for _ in 0..<120 {
                try? await Task.sleep(nanoseconds: 500_000_000)
                if Self.probe() { up = true; break }
            }
            if !up { throw BackendError.serverUnavailable }
        }

        let fd = try Self.connect()
        let readHandle = FileHandle(fileDescriptor: fd, closeOnDealloc: false)
        writeHandle = FileHandle(fileDescriptor: dup(fd), closeOnDealloc: false)

        let (stream, continuation) = AsyncStream.makeStream(of: [String: Any].self)
        eventContinuation = continuation

        Task.detached { [weak self] in
            do {
                for try await line in readHandle.bytes.lines {
                    guard let data = line.data(using: .utf8),
                          let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
                    else { continue }
                    await self?.route(obj)
                }
            } catch {}
            try? readHandle.close()
            await self?.handleDisconnect()
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

    private func handleDisconnect() {
        writeHandle = nil
        for (_, cont) in pending { cont.resume(throwing: BackendError.notConnected) }
        pending.removeAll()
        eventContinuation?.finish()
        eventContinuation = nil
    }

    func request(_ body: [String: Any]) async throws -> [String: Any] {
        guard let writer = writeHandle else { throw BackendError.notConnected }
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

    /// Ask the server to shut down (used by the menu bar's Stop item).
    func stopServer() async {
        _ = try? await request(["cmd": "shutdown"])
    }

    // Typed helpers ------------------------------------------------------

    func listGames() async throws -> [Game] {
        let resp = try await request(["cmd": "list-games"])
        let raw = resp["games"] ?? []
        return try JSONDecoder().decode([Game].self,
            from: JSONSerialization.data(withJSONObject: raw))
    }

    func friends() async throws -> [Friend] {
        let resp = try await request(["cmd": "friends"])
        let raw = resp["friends"] ?? []
        return try JSONDecoder().decode([Friend].self,
            from: JSONSerialization.data(withJSONObject: raw))
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

    // Transport ----------------------------------------------------------

    /// True if a server answers on the control port.
    nonisolated static func probe() -> Bool {
        guard let fd = try? connect() else { return false }
        Darwin.close(fd)
        return true
    }

    /// Open a blocking TCP connection to 127.0.0.1:port; returns the fd.
    nonisolated static func connect() throws -> Int32 {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw BackendError.notConnected }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard rc == 0 else {
            Darwin.close(fd)
            throw BackendError.notConnected
        }
        return fd
    }

    /// Spawn `maxima-cli server` detached so it outlives this app.
    private func spawnServer() throws {
        guard let cli = MaximaCLI.locate() else { throw BackendError.cliNotFound }
        let p = Process()
        p.executableURL = cli
        p.arguments = ["server"]
        p.standardInput = FileHandle.nullDevice
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        var env = ProcessInfo.processInfo.environment
        let wine = UserDefaults.standard.string(forKey: "wineCommand") ?? ""
        if !wine.isEmpty { env["MAXIMA_WINE_COMMAND"] = wine }
        p.environment = env
        try p.run()
    }
}
