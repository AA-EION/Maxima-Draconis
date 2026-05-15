import Cocoa
import Foundation
import os.log

// MaximaHelper — silent background agent for macOS/CrossOver login flow.
//
// Registered as the qrc:// URL scheme handler by Draconis on first setup.
// When EA's OAuth flow redirects to qrc://, macOS launches this app with the
// URL as an Apple Event. MaximaHelper forwards it to Maxima's TCP listener
// inside the CrossOver/Wine bottle.
//
// Networking note: Wine uses the macOS host's TCP stack, so 127.0.0.1:31033
// on the Mac reaches the same port that maxima-cli.exe binds inside the
// bottle. No special routing or proxy is needed.

private let log = OSLog(subsystem: "com.armchairdevelopers.maxima.helper", category: "forward")
private let maximaPort = 31033

class AppDelegate: NSObject, NSApplicationDelegate {
    private var pendingTask: URLSessionDataTask?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSAppleEventManager.shared().setEventHandler(
            self,
            andSelector: #selector(handleGetURL(_:withReply:)),
            forEventClass: AEEventClass(kInternetEventClass),
            andEventID: AEEventID(kAEGetURL)
        )
    }

    @objc func handleGetURL(
        _ event: NSAppleEventDescriptor,
        withReply reply: NSAppleEventDescriptor
    ) {
        guard
            let rawURL = event.paramDescriptor(forKeyword: keyDirectObject)?.stringValue,
            let url = URL(string: rawURL),
            url.scheme == "qrc"
        else {
            os_log("Ignoring non-qrc URL", log: log, type: .debug)
            NSApp.terminate(nil)
            return
        }

        os_log("Received qrc:// URL, forwarding to maxima-cli at 127.0.0.1:%d",
               log: log, type: .info, maximaPort)
        forward(url)
    }

    private func forward(_ url: URL) {
        guard
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let query = components.query,
            let target = URL(string: "http://127.0.0.1:\(maximaPort)/auth?\(query)")
        else {
            os_log("Malformed qrc:// URL — could not extract query string",
                   log: log, type: .error)
            NSApp.terminate(nil)
            return
        }

        var request = URLRequest(url: target, timeoutInterval: 8)
        request.httpMethod = "GET"

        pendingTask = URLSession.shared.dataTask(with: request) { [weak self] _, response, error in
            if let error = error {
                // maxima-cli may not be listening yet (e.g. auth window closed);
                // log and exit cleanly — the user can re-authenticate from the CLI.
                os_log("Forward failed: %{public}@ — is maxima-cli running in CrossOver?",
                       log: log, type: .error, error.localizedDescription)
            } else {
                os_log("Forward succeeded (HTTP %d)",
                       log: log, type: .info,
                       (response as? HTTPURLResponse)?.statusCode ?? 0)
            }
            DispatchQueue.main.async {
                self?.pendingTask = nil
                NSApp.terminate(nil)
            }
        }
        pendingTask?.resume()
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
