import AppKit
import Foundation

/// Maxima.app is the macOS handler for qrc://, link2ea:// and origin2://.
enum ProtocolHandler {
    static let schemes = ["qrc", "link2ea", "origin2"]

    static func register() async throws {
        for scheme in schemes {
            try await NSWorkspace.shared.setDefaultApplication(
                at: Bundle.main.bundleURL, toOpenURLsWithScheme: scheme)
        }
    }

    static func handle(_ raw: String) async {
        let lower = raw.lowercased()
        if lower.hasPrefix("qrc:") {
            await forwardLogin(raw)
        } else if lower.hasPrefix("link2ea:"), let url = URL(string: raw) {
            let offer = url.pathComponents.dropFirst().first ?? ""
            await authorize(offer: offer, cmdParams: rawQueryValue(url, "cmdParams"))
        } else if lower.hasPrefix("origin2:"), let url = URL(string: raw) {
            await authorize(offer: rawQueryValue(url, "offerIds") ?? "",
                            cmdParams: rawQueryValue(url, "cmdParams"))
        }
    }

    // Same forwarding as maxima-bootstrap: values stay percent-encoded and are encoded once more.
    private static func rawQueryValue(_ url: URL, _ name: String) -> String? {
        URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .percentEncodedQueryItems?.first { $0.name == name }?.value
    }

    private static func forwardLogin(_ raw: String) async {
        guard let marker = raw.range(of: "login_successful.html?"),
              let target = URL(string: "http://127.0.0.1:31033/auth?" + raw[marker.upperBound...])
        else { return }
        _ = try? await URLSession.shared.data(from: target)
    }

    private static func authorize(offer: String, cmdParams: String?) async {
        guard offer.range(of: #"^(Origin\.OFR\.\d+\.\d+|\d{1,10})$"#, options: .regularExpression) != nil
        else { return }
        if Backend.instance()?.authorizePort == nil {
            let backend = Backend()
            if (try? await backend.start(force: true)) != nil {
                _ = try? await backend.request(["cmd": "login"])
            }
        }

        // /authorize comes up once the server's EA login is done.
        var found: Backend.Instance?
        for _ in 0..<600 {
            if let instance = Backend.instance(), instance.authorizePort != nil {
                found = instance
                break
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
        guard let instance = found, let port = instance.authorizePort else { return }

        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        var query = "offer_id=" + (offer.addingPercentEncoding(withAllowedCharacters: allowed) ?? offer)
        if let params = cmdParams?.addingPercentEncoding(withAllowedCharacters: allowed) {
            query += "&cmd_params=" + params
        }
        guard let url = URL(string: "http://127.0.0.1:\(port)/authorize?\(query)") else { return }
        var request = URLRequest(url: url, timeoutInterval: 60)
        request.httpMethod = "POST"
        request.setValue(instance.token, forHTTPHeaderField: "x-maxima-token")
        _ = try? await URLSession.shared.data(for: request)
    }
}
