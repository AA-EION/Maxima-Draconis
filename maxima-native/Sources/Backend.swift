import Darwin
import Foundation

/// Client for the multi-client `maxima-server` (upstream PR #23's
/// server/thin-client architecture). Finds the server through the
/// `instance.json` it publishes in Maxima's data directory (its port and a
/// per-run token), and opens every connection with a `hello` carrying that
/// token. If no server is running it spawns one detached and waits for it.
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

    /// Maxima's data directory, shared with every Rust frontend
    /// (`MAXIMA_DATA_DIR` overrides it, as it does for them).
    nonisolated static var dataDir: URL {
        if let dir = ProcessInfo.processInfo.environment["MAXIMA_DATA_DIR"], !dir.isEmpty {
            return URL(fileURLWithPath: dir)
        }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/com.ArmchairDevelopers.Maxima")
    }

    /// Control-protocol version this client speaks (`maxima_proto::PROTO_VERSION`).
    static let protoVersion = 3

    /// Where the running server listens and the token it expects.
    struct Instance {
        let port: UInt16
        let token: String
        let authorizePort: UInt16?
    }

    /// The running server's `instance.json`, or nil when no server holds the
    /// instance lock (a file left behind by a crash doesn't count).
    nonisolated static func instance() -> Instance? {
        let lock = Darwin.open(dataDir.appendingPathComponent("instance.lock").path, O_RDWR)
        guard lock >= 0 else { return nil }
        defer { Darwin.close(lock) }
        if flock(lock, LOCK_EX | LOCK_NB) == 0 {
            flock(lock, LOCK_UN)
            return nil
        }
        guard let data = try? Data(contentsOf: dataDir.appendingPathComponent("instance.json")),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let port = (obj["control_port"] as? NSNumber)?.uint16Value,
              let token = obj["token"] as? String
        else { return nil }
        return Instance(port: port, token: token,
                        authorizePort: (obj["authorize_port"] as? NSNumber)?.uint16Value)
    }

    private var writeHandle: FileHandle?
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<[String: Any], Swift.Error>] = [:]
    private var eventContinuation: AsyncStream<[String: Any]>.Continuation?

    var isConnected: Bool { writeHandle != nil }

    /// Boot policy from the Maxima config (shared with the CLI / egui). Governs
    /// whether opening this app may auto-spawn the server. `manual` means the
    /// user starts it explicitly, so we don't spawn unless `force` is set (the
    /// "Start Server" action).
    nonisolated static func bootPolicy() -> String {
        let url = dataDir.appendingPathComponent("config.json")
        guard let data = try? Data(contentsOf: url),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let p = obj["boot_policy"] as? String
        else { return "on-demand" }
        return p
    }

    /// Ensure a server is up (spawn if needed, unless the boot policy is
    /// `manual` and `force` is false), connect, and return the pushed-event
    /// stream. The stream ends if the connection drops.
    func start(force: Bool = false) async throws -> AsyncStream<[String: Any]> {
        var found = Self.instance()
        if found == nil {
            if !force && Self.bootPolicy() == "manual" {
                // Manual policy: don't auto-spawn — surface as stopped so the UI
                // can offer a "Start Server" action.
                throw BackendError.serverUnavailable
            }
            try Self.spawnServer()
            // The server publishes its port before logging in, so this is quick.
            for _ in 0..<60 where found == nil {
                try? await Task.sleep(nanoseconds: 500_000_000)
                found = Self.instance()
            }
        }
        guard let instance = found else { throw BackendError.serverUnavailable }

        let fd = try Self.connect(port: instance.port)
        let readHandle = FileHandle(fileDescriptor: fd, closeOnDealloc: false)
        // closeOnDealloc: true so the dup'd fd is closed when writeHandle is
        // released (handleDisconnect sets it to nil) — otherwise it leaks.
        writeHandle = FileHandle(fileDescriptor: dup(fd), closeOnDealloc: true)

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

        // The server answers nothing else until the connection identifies.
        _ = try await request([
            "cmd": "hello", "token": instance.token,
            "client": "Maxima.app", "proto": Self.protoVersion,
        ])
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

    /// Open a blocking TCP connection to 127.0.0.1:port; returns the fd.
    nonisolated static func connect(port: UInt16) throws -> Int32 {
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

    /// Spawn `maxima-server` **fully detached** so it outlives this app.
    ///
    /// `Process`/NSTask leaves the child inside the app's launchd job, so
    /// macOS reaps it when the app quits — that's the "server dies when I close
    /// the window" bug. `posix_spawn` with `POSIX_SPAWN_SETSID` puts the server
    /// in its own session (its own launchd job context), independent of this
    /// app's lifecycle. This is only the *fallback* path — when the launchd
    /// service is installed (Settings → boot policy), the server is owned by
    /// launchd and this never runs.
    private nonisolated static func spawnServer() throws {
        guard let server = MaximaCLI.locateServer() else { throw BackendError.cliNotFound }
        let path = server.path

        // NB: `posix_spawn_file_actions_t` is `UnsafeMutableRawPointer` (non-
        // optional) on Darwin, and `_init` takes `UnsafeMutablePointer<…?>`, so
        // this MUST be declared optional — it is not a redundant double-optional.
        var fileActions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&fileActions)
        defer { posix_spawn_file_actions_destroy(&fileActions) }
        posix_spawn_file_actions_addopen(&fileActions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_addopen(&fileActions, 1, "/dev/null", O_WRONLY, 0)
        posix_spawn_file_actions_addopen(&fileActions, 2, "/dev/null", O_WRONLY, 0)

        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        // SETSID: new session → survives the app. CLOEXEC_DEFAULT: don't leak
        // the app's fds (TCP sockets, etc.) into the server.
        let POSIX_SPAWN_SETSID: Int16 = 0x0400
        let POSIX_SPAWN_CLOEXEC_DEFAULT: Int16 = 0x4000
        posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT)

        var env = ProcessInfo.processInfo.environment
        let wine = UserDefaults.standard.string(forKey: "wineCommand") ?? ""
        if !wine.isEmpty { env["MAXIMA_WINE_COMMAND"] = wine }
        let envStrings = env.map { "\($0.key)=\($0.value)" }

        var pid: pid_t = 0
        let rc = Self.withCStringArray([path]) { argv in
            Self.withCStringArray(envStrings) { envp in
                posix_spawn(&pid, path, &fileActions, &attr, argv, envp)
            }
        }
        guard rc == 0 else { throw BackendError.serverUnavailable }
    }

    /// Build a NULL-terminated C string array for posix_spawn, freeing the
    /// dup'd strings after `body` returns.
    private nonisolated static func withCStringArray<R>(
        _ strings: [String], _ body: (UnsafePointer<UnsafeMutablePointer<CChar>?>) -> R
    ) -> R {
        var cStrings: [UnsafeMutablePointer<CChar>?] = strings.map { strdup($0) }
        cStrings.append(nil)
        defer { cStrings.forEach { free($0) } }
        return body(&cStrings)
    }
}
