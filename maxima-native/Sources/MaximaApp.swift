import AppKit
import SwiftUI

/// Maxima's brand orange — same value as the egui UI's `F9B233` accent.
let maximaOrange = Color(red: 249 / 255, green: 178 / 255, blue: 51 / 255)

/// The server's status-bar mark: a solid circle with the letter **M** knocked
/// out of it (the M is the negative/transparent part). Drawn as a template
/// image so the menu bar tints the circle to match light/dark automatically.
func maximaStatusIcon() -> NSImage {
    let size = NSSize(width: 18, height: 18)
    let image = NSImage(size: size, flipped: false) { rect in
        guard let ctx = NSGraphicsContext.current?.cgContext else { return false }
        // Positive: the solid circle.
        ctx.setFillColor(NSColor.black.cgColor)
        ctx.fillEllipse(in: rect.insetBy(dx: 1, dy: 1))
        // Negative: cut the M out of the circle.
        ctx.setBlendMode(.destinationOut)
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: rect.height * 0.6, weight: .heavy),
            .foregroundColor: NSColor.black,
        ]
        let m = "M" as NSString
        let ms = m.size(withAttributes: attrs)
        m.draw(
            at: NSPoint(x: rect.midX - ms.width / 2, y: rect.midY - ms.height / 2),
            withAttributes: attrs)
        return true
    }
    image.isTemplate = true
    return image
}

/// Held for the app's lifetime: Maxima.app must never App Nap. macOS
/// responsibility-attributes the backend — and transitively the wine/game
/// tree — to this app, and a napped responsible app background-throttles
/// the whole tree (the game renders a frozen blank window). Public-API
/// safety net alongside CleanSpawn's responsibility disclaim.
private let napPreventionToken: NSObjectProtocol = ProcessInfo.processInfo.beginActivity(
    options: [.userInitiated, .idleSystemSleepDisabled],
    reason: "Maxima keeps its game session backend responsive"
)

@main
struct MaximaApp: App {
    @StateObject private var store = GameStore()
    @Environment(\.openWindow) private var openWindow

    init() {
        _ = napPreventionToken
    }

    var body: some Scene {
        WindowGroup(id: "main") {
            ContentView()
                .environmentObject(store)
                .frame(minWidth: 960, minHeight: 600)
                .task { store.start() }
        }

        // macOS's idiomatic "bar icon": a menu-bar extra reflecting the
        // shared server, with the same three actions the Windows tray and
        // the CLI expose — open the UI, stop the server, quit. (Windows uses
        // the native tray inside the server; Linux runs headless.)
        MenuBarExtra {
            menuBarContent
        } label: {
            Image(nsImage: maximaStatusIcon())
        }
    }

    @ViewBuilder private var menuBarContent: some View {
        Group {
            switch store.backendState {
            case .ready(let persona):
                Text(persona.isEmpty ? "Connected" : "Signed in as \(persona)")
            case .connecting:
                Text("Connecting…")
            case .stopped:
                Text("Server stopped")
            }
            Divider()
            Button("Open Maxima") {
                NSApp.activate(ignoringOtherApps: true)
                openWindow(id: "main")
            }
            Button("Stop Server") { store.stopServer() }
            Divider()
            Button("Quit Maxima") { NSApp.terminate(nil) }
        }
    }
}

enum SidebarItem: String, CaseIterable, Identifiable {
    case library = "Library"
    case downloads = "Downloads"
    case friends = "Friends"
    case settings = "Settings"

    var id: String { rawValue }

    var icon: String {
        switch self {
        case .library: return "square.grid.2x2"
        case .downloads: return "arrow.down.circle"
        case .friends: return "person.2"
        case .settings: return "gearshape"
        }
    }
}

struct ContentView: View {
    @State private var selection: SidebarItem? = .library
    @EnvironmentObject var store: GameStore

    var body: some View {
        NavigationSplitView {
            List(selection: $selection) {
                ForEach(SidebarItem.allCases) { item in
                    Label {
                        HStack {
                            Text(item.rawValue)
                            Spacer()
                            badge(for: item)
                        }
                    } icon: {
                        Image(systemName: item.icon)
                    }
                    .tag(item)
                }
            }
            .navigationSplitViewColumnWidth(min: 190, ideal: 210)
            .safeAreaInset(edge: .bottom) {
                connectionFooter
            }
        } detail: {
            switch selection ?? .library {
            case .library: LibraryView()
            case .downloads: DownloadsView()
            case .friends: FriendsView()
            case .settings: SettingsView()
            }
        }
        .navigationTitle("Maxima")
        .alert(
            "Maxima",
            isPresented: Binding(
                get: { store.errorMessage != nil },
                set: { if !$0 { store.errorMessage = nil } }
            )
        ) {
            Button("OK") { store.errorMessage = nil }
        } message: {
            Text(store.errorMessage ?? "")
        }
    }

    @ViewBuilder
    private func badge(for item: SidebarItem) -> some View {
        switch item {
        case .downloads where !store.activeInstalls.isEmpty:
            Text("\(store.activeInstalls.count)")
                .font(.caption2.weight(.semibold))
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(maximaOrange.opacity(0.25), in: .capsule)
        case .friends where store.onlineFriendCount > 0:
            Text("\(store.onlineFriendCount)")
                .font(.caption2.weight(.semibold))
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(Color.green.opacity(0.22), in: .capsule)
        default:
            EmptyView()
        }
    }

    @ViewBuilder
    private var connectionFooter: some View {
        HStack(spacing: 6) {
            switch store.backendState {
            case .connecting:
                ProgressView().controlSize(.mini)
                Text("Connecting…").font(.caption)
            case .ready(let persona):
                Circle().fill(.green).frame(width: 7, height: 7)
                Text(persona.isEmpty ? "Connected" : persona)
                    .font(.caption)
                    .lineLimit(1)
            case .stopped(let reason):
                Circle().fill(.red).frame(width: 7, height: 7)
                Text(reason ?? "Backend stopped")
                    .font(.caption)
                    .lineLimit(1)
                Button {
                    store.start()
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .controlSize(.mini)
            }
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

/// Shared dark backdrop — tuned to the egui UI's near-black blue palette so
/// the glass surfaces have something to refract.
struct MaximaBackground: View {
    var body: some View {
        LinearGradient(
            colors: [
                Color(red: 0.06, green: 0.08, blue: 0.13),
                Color(red: 0.02, green: 0.03, blue: 0.05),
            ],
            startPoint: .top,
            endPoint: .bottom
        )
        .ignoresSafeArea()
    }
}
