import AppKit
import SwiftUI

struct SettingsView: View {
    @AppStorage("wineCommand") private var wineCommand = ""
    @AppStorage("maximaCliPath") private var cliPath = ""
    @State private var registerResult: String?
    @State private var registering = false

    private var crossOverWine: String {
        "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/wine"
    }

    private var engineStatus: String {
        if !wineCommand.isEmpty {
            return "Engine: custom command"
        }
        return FileManager.default.fileExists(atPath: crossOverWine)
            ? "Engine: CrossOver (auto-detected)"
            : "Engine: none found — install CrossOver or set a custom command"
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                section("Wine engine", icon: "wineglass") {
                    Text(engineStatus)
                        .font(.callout)
                    HStack {
                        TextField(
                            "Custom wine command — empty for auto (CrossOver)",
                            text: $wineCommand
                        )
                        .textFieldStyle(.roundedBorder)
                        Button("Browse…") { browseWine() }
                            .buttonStyle(.glass)
                    }
                    Divider()
                    HStack {
                        VStack(alignment: .leading, spacing: 2) {
                            Text("CrossOver bottles folder")
                                .font(.callout)
                            Text(GameStore.bottlesRoot().path)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .textSelection(.enabled)
                        }
                        Spacer()
                        Button("Open in Finder") {
                            NSWorkspace.shared.open(GameStore.bottlesRoot())
                        }
                        .buttonStyle(.glass)
                    }
                    Text("Games install into per-game bottles named Maxima-<slug>. A custom bottles location set in CrossOver's preferences is honored automatically.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                section("URL protocols", icon: "link") {
                    Text("Registers qrc://, link2ea:// and origin2:// with macOS so EA login redirects and externally-launched games reach Maxima.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    HStack {
                        Button {
                            register()
                        } label: {
                            if registering {
                                ProgressView().controlSize(.small)
                            } else {
                                Text("Register URL Handlers")
                            }
                        }
                        .buttonStyle(.glass)
                        .disabled(registering)
                        if let result = registerResult {
                            Text(result)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }

                section("maxima-cli", icon: "terminal") {
                    TextField("Path override — empty for auto-detect", text: $cliPath)
                        .textFieldStyle(.roundedBorder)
                    Text("Using: \(MaximaCLI.locate()?.path ?? "NOT FOUND — build maxima-cli or set a path above")")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
            .padding(24)
            .frame(maxWidth: 720, alignment: .leading)
        }
        .background(MaximaBackground())
    }

    @ViewBuilder
    private func section<Content: View>(
        _ title: String,
        icon: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Label(title, systemImage: icon)
                .font(.headline)
                .foregroundStyle(maximaOrange)
            content()
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .glassEffect(.regular, in: .rect(cornerRadius: 16))
    }

    private func browseWine() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "Choose a wine binary"
        if panel.runModal() == .OK, let url = panel.url {
            wineCommand = url.path
        }
    }

    private func register() {
        registering = true
        registerResult = nil
        Task {
            do {
                try await MaximaCLI.registerProtocols()
                registerResult = "Registered ✓"
            } catch {
                registerResult = error.localizedDescription
            }
            registering = false
        }
    }
}
