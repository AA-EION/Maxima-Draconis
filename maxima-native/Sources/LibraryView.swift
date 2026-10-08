import SwiftUI

struct LibraryView: View {
    @EnvironmentObject var store: GameStore

    private let columns = [GridItem(.adaptive(minimum: 270), spacing: 16)]

    var body: some View {
        ScrollView {
            switch store.backendState {
            case .connecting:
                VStack(spacing: 14) {
                    ProgressView()
                        .controlSize(.large)
                    Text("Signing in to EA…")
                        .font(.title3)
                    Text("A browser window may open to complete the login.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity)
                .padding(.top, 140)
            case .stopped(let reason):
                VStack(spacing: 12) {
                    Image(systemName: "exclamationmark.triangle")
                        .font(.system(size: 36))
                        .foregroundStyle(maximaOrange)
                    Text(reason ?? "Maxima backend stopped")
                        .font(.title3)
                    Button("Reconnect") { store.start() }
                        .buttonStyle(.glassProminent)
                        .tint(maximaOrange)
                }
                .frame(maxWidth: .infinity)
                .padding(.top, 140)
            case .ready:
                if store.games.isEmpty && store.loading {
                    ProgressView()
                        .frame(maxWidth: .infinity)
                        .padding(.top, 140)
                } else {
                    GlassEffectContainer {
                        LazyVGrid(columns: columns, spacing: 16) {
                            ForEach(store.games) { game in
                                GameCard(game: game)
                            }
                        }
                        .padding(20)
                    }
                }
            }
        }
        .background(MaximaBackground())
        .toolbar {
            ToolbarItem {
                Button {
                    Task { await store.refreshAll() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .disabled(store.loading)
                .help("Refresh library")
            }
        }
    }
}

struct GameCard: View {
    @EnvironmentObject var store: GameStore
    let game: Game
    @State private var showSettings = false

    private var status: GameStatus { store.statuses[game.slug] ?? .unknown }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                Image(systemName: "gamecontroller.fill")
                    .font(.system(size: 26))
                    .foregroundStyle(maximaOrange)
                Spacer()
                statusPill
                Button {
                    showSettings = true
                } label: {
                    Image(systemName: "gearshape")
                        .font(.system(size: 13))
                }
                .buttonStyle(.borderless)
                .help("Game settings")
            }
            Text(game.displayName.isEmpty ? game.name : game.displayName)
                .font(.title3.weight(.semibold))
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
            Text(game.slug)
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer(minLength: 6)
            actionRow
        }
        .padding(16)
        .frame(maxWidth: .infinity, minHeight: 160, alignment: .leading)
        .glassEffect(.regular, in: .rect(cornerRadius: 16))
        .sheet(isPresented: $showSettings) {
            GameSettingsSheet(game: game)
                .environmentObject(store)
        }
    }

    @ViewBuilder private var statusPill: some View {
        switch status {
        case .installed:
            pill("Installed", color: .green)
        case .running:
            pill("Running", color: maximaOrange)
        case .installing:
            pill("Installing", color: .blue)
        case .notInstalled, .unknown:
            pill("Not installed", color: .secondary)
        }
    }

    private func pill(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.medium))
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(color.opacity(0.18), in: .capsule)
            .foregroundStyle(color)
    }

    @ViewBuilder private var actionRow: some View {
        switch status {
        case .installed:
            Button {
                store.play(game)
            } label: {
                Label("Play", systemImage: "play.fill")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glassProminent)
            .tint(maximaOrange)
        case .notInstalled, .unknown:
            Button {
                store.install(game)
            } label: {
                Label("Install", systemImage: "arrow.down.circle")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.glass)
        case .installing(let pct):
            VStack(alignment: .leading, spacing: 6) {
                ProgressView(value: min(max(pct, 0), 100), total: 100)
                    .tint(maximaOrange)
                Text(String(format: "Downloading… %.1f%%", pct))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .running:
            Label("Game is running", systemImage: "circle.fill")
                .font(.callout)
                .foregroundStyle(.green)
                .frame(maxWidth: .infinity)
        }
    }
}

/// Per-game launch preferences — feature parity with the egui UI's
/// game-settings modal (launch args, exe override, cloud saves).
struct GameSettingsSheet: View {
    @EnvironmentObject var store: GameStore
    @Environment(\.dismiss) private var dismiss
    let game: Game

    @State private var settings = GameLocalSettings()

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(game.displayName.isEmpty ? game.name : game.displayName)
                .font(.title2.weight(.semibold))

            VStack(alignment: .leading, spacing: 6) {
                Text("Launch arguments")
                    .font(.callout)
                TextField("-novid -northstar …", text: $settings.launchArgs)
                    .textFieldStyle(.roundedBorder)
                Text("Passed to the game verbatim, space-separated.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Executable override")
                    .font(.callout)
                TextField("Empty = auto (bottle install dir)", text: $settings.exeOverride)
                    .textFieldStyle(.roundedBorder)
            }

            Toggle("Sync cloud saves", isOn: $settings.cloudSaves)
                .disabled(!game.hasCloudSave)

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .buttonStyle(.glass)
                Button("Save") {
                    store.setLocalSettings(settings, for: game.slug)
                    dismiss()
                }
                .buttonStyle(.glassProminent)
                .tint(maximaOrange)
            }
        }
        .padding(24)
        .frame(width: 460)
        .onAppear {
            settings = store.localSettings(for: game.slug)
        }
    }
}
