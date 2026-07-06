import AppKit
import SwiftUI

struct SettingsView: View {
    @AppStorage("wineCommand") private var wineCommand = ""
    @AppStorage("maximaCliPath") private var cliPath = ""
    @State private var registerResult: String?
    @State private var registering = false
    @State private var bootPolicy = Backend.bootPolicy()
    @State private var serviceBusy = false
    @State private var serviceResult: String?
    @State private var showUninstallConfirm = false

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

                section("Background service", icon: "gearshape.2") {
                    Text("The Maxima server runs independently of this app — closing the window doesn't stop it. Choose when it starts.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    Picker("Start the server", selection: $bootPolicy) {
                        Text("At login — always running").tag("auto")
                        Text("When a game or app opens").tag("on-demand")
                        Text("Only when I start it").tag("manual")
                    }
                    .pickerStyle(.radioGroup)
                    .disabled(serviceBusy)
                    .onChange(of: bootPolicy) { _, new in applyBootPolicy(new) }

                    HStack {
                        if serviceBusy { ProgressView().controlSize(.small) }
                        if let result = serviceResult {
                            Text(result).font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Button("Uninstall service…", role: .destructive) {
                            showUninstallConfirm = true
                        }
                        .buttonStyle(.glass)
                        .disabled(serviceBusy)
                    }
                    Text("Uninstall removes the autostart, protocol registrations and installed binaries, leaving no trace that could interfere with the official EA app.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .confirmationDialog(
                    "Uninstall the Maxima background service?",
                    isPresented: $showUninstallConfirm,
                    titleVisibility: .visible
                ) {
                    Button("Uninstall", role: .destructive) { uninstallService(purge: false) }
                    Button("Uninstall & delete login/data", role: .destructive) {
                        uninstallService(purge: true)
                    }
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text("The server will stop and its autostart, protocol claims and binaries will be removed. Game bottles are kept.")
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

    private func applyBootPolicy(_ policy: String) {
        serviceBusy = true
        serviceResult = nil
        Task {
            do {
                try await MaximaCLI.serviceInstall(boot: policy)
                serviceResult = "Saved ✓"
            } catch {
                serviceResult = error.localizedDescription
            }
            serviceBusy = false
        }
    }

    private func uninstallService(purge: Bool) {
        serviceBusy = true
        serviceResult = nil
        Task {
            do {
                try await MaximaCLI.serviceUninstall(purge: purge)
                serviceResult = "Uninstalled ✓"
            } catch {
                serviceResult = error.localizedDescription
            }
            serviceBusy = false
        }
    }
}
