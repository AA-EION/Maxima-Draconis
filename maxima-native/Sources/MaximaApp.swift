import SwiftUI

/// Maxima's brand orange — same value as the egui UI's `F9B233` accent.
let maximaOrange = Color(red: 249 / 255, green: 178 / 255, blue: 51 / 255)

@main
struct MaximaApp: App {
    @StateObject private var store = GameStore()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(store)
                .frame(minWidth: 900, minHeight: 560)
        }
    }
}

enum SidebarItem: String, CaseIterable, Identifiable {
    case library = "Library"
    case settings = "Settings"

    var id: String { rawValue }

    var icon: String {
        switch self {
        case .library: return "square.grid.2x2"
        case .settings: return "gearshape"
        }
    }
}

struct ContentView: View {
    @State private var selection: SidebarItem? = .library

    var body: some View {
        NavigationSplitView {
            List(SidebarItem.allCases, selection: $selection) { item in
                Label(item.rawValue, systemImage: item.icon)
                    .tag(item)
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 200)
        } detail: {
            switch selection ?? .library {
            case .library: LibraryView()
            case .settings: SettingsView()
            }
        }
        .navigationTitle("Maxima")
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
