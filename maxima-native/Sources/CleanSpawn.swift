import Darwin
import Foundation

/// `posix_spawn` wrapper that gives a child process the same clean context a
/// Terminal-spawned command gets. Technique established by Draconis's
/// CleanSpawn (see that repo's Services/CleanSpawn.swift), which root-caused
/// a reproducible failure: wine children spawned via `Foundation.Process`
/// from a `.app` inherit the app's non-CLOEXEC descriptors, session and App
/// Nap responsibility — and Titanfall 2 freezes right after LSX
/// `GetAllGameInfo`. The same chain works from Terminal.
///
/// Three fixes applied:
///  * `POSIX_SPAWN_CLOEXEC_DEFAULT` — child gets ONLY the descriptors we
///    dup2 explicitly (our stdio pipes), not AppKit's Mach/IOSurface fds.
///  * `POSIX_SPAWN_SETSID` — child leads its own session, like a shell job.
///  * `responsibility_spawnattrs_setdisclaim` (private, via dlsym, best
///    effort) — child is its own "responsible process", so the app going
///    to background doesn't App-Nap the wine tree.
enum CleanSpawn {
    struct Error: Swift.Error, LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    // Not imported into Swift's Darwin module; values from <spawn.h>.
    private static let POSIX_SPAWN_CLOEXEC_DEFAULT: Int16 = 0x4000
    private static let POSIX_SPAWN_SETSID: Int16 = 0x0400

    private typealias DisclaimFn = @convention(c) (UnsafeMutablePointer<posix_spawnattr_t?>, Int32) -> Int32
    private static let disclaim: DisclaimFn? = {
        guard let sym = dlsym(
            UnsafeMutableRawPointer(bitPattern: -2), // RTLD_DEFAULT
            "responsibility_spawnattrs_setdisclaim"
        ) else { return nil }
        return unsafeBitCast(sym, to: DisclaimFn.self)
    }()

    /// Spawn `executable` with the given stdio descriptors dup2'd to 0/1/2.
    /// Returns the child pid.
    static func spawn(
        executable: URL,
        arguments: [String],
        environment: [String: String],
        stdinFD: Int32,
        stdoutFD: Int32,
        stderrFD: Int32
    ) throws -> pid_t {
        var attr: posix_spawnattr_t?
        guard posix_spawnattr_init(&attr) == 0 else {
            throw Error(message: "posix_spawnattr_init failed")
        }
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSID)
        _ = CleanSpawn.disclaim?(&attr, 1) // best effort; private API

        var actions: posix_spawn_file_actions_t?
        guard posix_spawn_file_actions_init(&actions) == 0 else {
            throw Error(message: "posix_spawn_file_actions_init failed")
        }
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_adddup2(&actions, stdinFD, 0)
        posix_spawn_file_actions_adddup2(&actions, stdoutFD, 1)
        posix_spawn_file_actions_adddup2(&actions, stderrFD, 2)

        let argv = [executable.path] + arguments
        var cArgv: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) }
        cArgv.append(nil)
        var cEnvp: [UnsafeMutablePointer<CChar>?] = environment.map { strdup("\($0.key)=\($0.value)") }
        cEnvp.append(nil)
        defer {
            cArgv.forEach { free($0) }
            cEnvp.forEach { free($0) }
        }

        var pid: pid_t = 0
        let rc = posix_spawn(&pid, executable.path, &actions, &attr, cArgv, cEnvp)
        guard rc == 0 else {
            throw Error(message: "posix_spawn failed: \(String(cString: strerror(rc)))")
        }
        return pid
    }
}
