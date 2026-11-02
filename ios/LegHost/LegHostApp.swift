import SwiftUI

/// The native iOS host for the leg guest.
///
/// The host embeds the pinned iSH revision vendored under `vendor/ish`, the
/// pinned guest bundle and the pinned Alpine root. On launch it verifies both
/// pinned inputs, imports them into a fakefs root and runs `leg-tui` on a
/// pseudo terminal.
@main
struct LegHostApp: App {
    @StateObject private var controller = GuestHostController()

    var body: some Scene {
        WindowGroup {
            GuestHostScreen(controller: controller)
                .onAppear { controller.start() }
        }
    }
}

/// The screen: the terminal once the guest runs, the progress before that.
struct GuestHostScreen: View {
    @ObservedObject var controller: GuestHostController

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            switch controller.phase {
            case .running:
                VStack(spacing: 0) {
                    statusBar
                    GuestTerminalView(controller: controller)
                }
            case .exited:
                statusView(title: "The guest program ended",
                          detail: controller.detail,
                          identifier: "guest-exited")
            case .failed(let message):
                statusView(title: "The guest could not start", detail: message)
            default:
                statusView(title: title, detail: controller.detail)
            }
        }
        .preferredColorScheme(.dark)
    }

    /// The bar above the terminal. It records whether this launch imported
    /// the guest root or reused the verified one, and the size the guest
    /// terminal was told about.
    private var statusBar: some View {
        HStack(spacing: 8) {
            Text(controller.reusedVerifiedRoot ? "reused verified root" : "imported root")
                .font(.caption2.monospaced())
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("guest-root-state")
            Spacer()
            if !controller.terminalSize.isEmpty {
                Text("\(controller.terminalSize.columns)x\(controller.terminalSize.rows)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("guest-terminal-size")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
    }

    private var title: String {
        switch controller.phase {
        case .preparing: return "Preparing the guest"
        case .provisioning: return "Verifying the pinned inputs"
        case .booting: return "Booting the guest"
        case .running: return "Running the guest"
        case .exited: return "The guest program ended"
        case .failed: return "The guest could not start"
        }
    }

    private func statusView(title: String, detail: String, identifier: String? = nil) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("LegHost")
                .font(.largeTitle.bold())
            Text(title)
                .font(.headline)
                .accessibilityIdentifier(identifier ?? "guest-status-title")
            if !detail.isEmpty {
                Text(detail)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            Divider()
            LabeledContent("Vendored iSH revision", value: HostBundle.ishRevision ?? "unknown")
            if controller.reusedVerifiedRoot {
                LabeledContent("Guest root", value: "reused the verified root")
            }
            if case .failed = controller.phase {
                Button("Try again") { controller.start() }
            }
            Spacer()
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

enum HostBundle {
    /// Revision marker of the vendored iSH runtime, bundled from
    /// `vendor/ish/ISH_REVISION`.
    static let ishRevision: String? = {
        guard let url = Bundle.main.url(forResource: "ISH_REVISION", withExtension: nil),
            let contents = try? String(contentsOf: url, encoding: .utf8)
        else {
            return nil
        }
        return contents.trimmingCharacters(in: .whitespacesAndNewlines)
    }()
}
