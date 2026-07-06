import SwiftUI

/// The persistent right-hand friends rail (Steam-style). Always visible
/// alongside the main content — online friends first, in-game status shown.
/// A better take on the egui UI, where friends live in a separate view.
struct FriendsSidebar: View {
    @EnvironmentObject var store: GameStore

    private var sortedFriends: [Friend] {
        store.friends.sorted { a, b in
            let aOnline = store.presences[a.id]?.isOnline == true
            let bOnline = store.presences[b.id]?.isOnline == true
            if aOnline != bOnline { return aOnline }
            return a.name.localizedCaseInsensitiveCompare(b.name) == .orderedAscending
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Label("Friends", systemImage: "person.2.fill")
                    .font(.headline)
                Spacer()
                if !store.friends.isEmpty {
                    Text("\(store.onlineFriendCount)/\(store.friends.count)")
                        .font(.caption.weight(.medium))
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 12)

            Divider()

            if store.friends.isEmpty {
                VStack(spacing: 8) {
                    Image(systemName: "person.2")
                        .font(.system(size: 28))
                        .foregroundStyle(.secondary)
                    Text("No friends online")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .padding(.top, 40)
            } else {
                ScrollView {
                    LazyVStack(spacing: 8) {
                        ForEach(sortedFriends) { friend in
                            friendRow(friend)
                        }
                    }
                    .padding(10)
                }
            }
        }
        .frame(width: 250)
        .background(.ultraThinMaterial)
    }

    private func friendRow(_ friend: Friend) -> some View {
        let presence = store.presences[friend.id]
        let online = presence?.isOnline == true

        return HStack(spacing: 10) {
            ZStack(alignment: .bottomTrailing) {
                Image(systemName: "person.crop.circle.fill")
                    .font(.system(size: 26))
                    .foregroundStyle(online ? maximaOrange : Color.secondary)
                Circle()
                    .fill(online ? Color.green : Color.gray)
                    .frame(width: 8, height: 8)
                    .overlay(Circle().stroke(.black.opacity(0.6), lineWidth: 1))
            }
            VStack(alignment: .leading, spacing: 1) {
                Text(friend.name)
                    .font(.callout.weight(.medium))
                    .lineLimit(1)
                if let game = presence?.game, !game.isEmpty {
                    Label(game, systemImage: "gamecontroller")
                        .font(.caption2)
                        .foregroundStyle(maximaOrange)
                        .lineLimit(1)
                } else if let status = presence?.status, !status.isEmpty {
                    Text(status)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                } else {
                    Text(online ? "Online" : "Offline")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassEffect(.regular, in: .rect(cornerRadius: 10))
        .opacity(online ? 1 : 0.5)
    }
}
