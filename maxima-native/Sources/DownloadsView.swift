import SwiftUI

struct DownloadsView: View {
    @EnvironmentObject var store: GameStore

    var body: some View {
        ScrollView {
            if store.activeInstalls.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "arrow.down.circle")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("No active downloads")
                        .font(.title3)
                    Text("Installs started from the Library appear here.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity)
                .padding(.top, 140)
            } else {
                GlassEffectContainer {
                    VStack(spacing: 14) {
                        ForEach(store.activeInstalls, id: \.game.slug) { entry in
                            downloadRow(entry.game, percent: entry.percent)
                        }
                    }
                    .padding(20)
                }
            }
        }
        .background(MaximaBackground())
    }

    private func downloadRow(_ game: Game, percent: Double) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Image(systemName: "gamecontroller.fill")
                    .foregroundStyle(maximaOrange)
                Text(game.displayName.isEmpty ? game.name : game.displayName)
                    .font(.headline)
                Spacer()
                Text(String(format: "%.1f%%", percent))
                    .font(.callout.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            ProgressView(value: min(max(percent, 0), 100), total: 100)
                .tint(maximaOrange)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassEffect(.regular, in: .rect(cornerRadius: 16))
    }
}
