import SwiftUI

struct LibraryView: View {
    @EnvironmentObject var store: GameStore

    private let columns = [GridItem(.adaptive(minimum: 270), spacing: 16)]

    var body: some View {
        ScrollView {
            if store.firstLoad {
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
            } else if store.games.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "square.grid.2x2")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("No games in your EA library")
                        .font(.title3)
                }
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
        .background(MaximaBackground())
        .toolbar {
            ToolbarItem {
                Button {
                    Task { await store.refresh() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .disabled(store.loading)
                .help("Refresh library")
            }
        }
        .task {
            if store.firstLoad {
                await store.refresh()
            }
        }
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
}

struct GameCard: View {
    @EnvironmentObject var store: GameStore
    let game: Game

    private var status: GameStatus { store.statuses[game.slug] ?? .unknown }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                Image(systemName: "gamecontroller.fill")
                    .font(.system(size: 26))
                    .foregroundStyle(maximaOrange)
                Spacer()
                statusPill
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
