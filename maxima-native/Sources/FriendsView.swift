import SwiftUI

struct FriendsView: View {
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
        ScrollView {
            if store.friends.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "person.2")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("No friends yet")
                        .font(.title3)
                }
                .frame(maxWidth: .infinity)
                .padding(.top, 140)
            } else {
                GlassEffectContainer {
                    VStack(spacing: 10) {
                        ForEach(sortedFriends) { friend in
                            friendRow(friend)
                        }
                    }
                    .padding(20)
                }
            }
        }
        .background(MaximaBackground())
        .navigationSubtitle("\(store.onlineFriendCount) of \(store.friends.count) online")
    }

    private func friendRow(_ friend: Friend) -> some View {
        let presence = store.presences[friend.id]
        let online = presence?.isOnline == true

        return HStack(spacing: 12) {
            ZStack(alignment: .bottomTrailing) {
                Image(systemName: "person.crop.circle.fill")
                    .font(.system(size: 30))
                    .foregroundStyle(online ? maximaOrange : Color.secondary)
                Circle()
                    .fill(online ? Color.green : Color.gray)
                    .frame(width: 9, height: 9)
                    .overlay(Circle().stroke(.black.opacity(0.6), lineWidth: 1))
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(friend.name)
                    .font(.body.weight(.medium))
                if let game = presence?.game, !game.isEmpty {
                    Label(game, systemImage: "gamecontroller")
                        .font(.caption)
                        .foregroundStyle(maximaOrange)
                } else if let status = presence?.status, !status.isEmpty {
                    Text(status)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    Text(online ? "Online" : "Offline")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassEffect(.regular, in: .rect(cornerRadius: 12))
        .opacity(online ? 1 : 0.55)
    }
}
