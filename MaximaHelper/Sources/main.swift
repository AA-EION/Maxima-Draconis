import Cocoa
import Foundation
import os.log

// MaximaHelper — silent background agent for macOS/CrossOver login flow.
//
// Registered as the qrc:// URL scheme handler by the consuming launcher on first setup.
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
    private var receivedURL = false

    // When macOS launches this app *for* a qrc:// link, the GetURL Apple
    // Event is delivered while the app is still launching, before
    // applicationDidFinishLaunching. Registering there lost that event: the
    // helper stayed running and nothing was forwarded. Register before launch
    // completes, as Apple recommends for URL handlers.
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSAppleEventManager.shared().setEventHandler(
            self,
            andSelector: #selector(handleGetURL(_:withReply:)),
            forEventClass: AEEventClass(kInternetEventClass),
            andEventID: AEEventID(kAEGetURL)
        )
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Launched without a link, or it never arrived: don't stay running.
        DispatchQueue.main.asyncAfter(deadline: .now() + 30) { [weak self] in
            guard let self, !self.receivedURL else { return }
            os_log("No qrc:// URL received; exiting", log: log, type: .error)
            NSApp.terminate(nil)
        }
    }

    // AppKit's own route for opened URLs, used if the Apple Event reaches it
    // instead of the handler above.
    func application(_ application: NSApplication, open urls: [URL]) {
        guard let url = urls.first else { return }
        handle(url.absoluteString)
    }

    @objc func handleGetURL(
        _ event: NSAppleEventDescriptor,
        withReply reply: NSAppleEventDescriptor
    ) {
        handle(event.paramDescriptor(forKeyword: keyDirectObject)?.stringValue)
    }

    private func handle(_ rawURL: String?) {
        guard !receivedURL else { return }
        guard
            let rawURL,
            let url = URL(string: rawURL),
            url.scheme == "qrc"
        else {
            os_log("Ignoring non-qrc URL", log: log, type: .error)
            NSApp.terminate(nil)
            return
        }
        receivedURL = true

        os_log("Received qrc:// URL, forwarding to Maxima at 127.0.0.1:%d",
               log: log, type: .default, maximaPort)
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
                       log: log, type: .default,
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
