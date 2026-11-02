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
                    stateRow
                    failureRow
                    controlRow
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

    /// What the host did in this run.
    ///
    /// The line holds the lifecycle counts and the fixture observation. It
    /// carries no request data and no credential.
    private var stateRow: some View {
        HStack(spacing: 8) {
            Text(controller.state.summary)
                .font(.caption2.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .minimumScaleFactor(0.5)
                .accessibilityIdentifier("guest-fixture-state")
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 2)
    }

    /// The failure the guest reported for the turn.
    ///
    /// The guest program records the failure of a turn in its exchange
    /// trail. The host shows what the guest reported there, so a failed
    /// turn is visible on the screen. The line is empty while the turn
    /// runs or when the guest reported no failure.
    @ViewBuilder
    private var failureRow: some View {
        if let failure = controller.guestTurnFailure {
            Text(failure)
                .font(.caption2.monospaced())
                .foregroundStyle(Color.red)
                .lineLimit(2)
                .accessibilityIdentifier("guest-turn-failure")
                .padding(.horizontal, 12)
                .padding(.vertical, 2)
        }
    }

    /// The keys the software keyboard cannot send.
    ///
    /// The row sits above the terminal, so it stays reachable while the
    /// software keyboard covers the lower half of the screen. Without it a
    /// guest program could not be interrupted without a hardware keyboard.
    private var controlRow: some View {
        HStack(spacing: 6) {
            ForEach(Self.controlKeys) { key in
                Button {
                    controller.sendInput(key.sequence)
                } label: {
                    Text(key.label)
                        .font(.caption.monospaced())
                        .frame(minWidth: 26)
                        .padding(.vertical, 2)
                        .background(Color.white.opacity(0.12))
                        .foregroundStyle(key.label == Self.interruptLabel ? Color.red : Color.white)
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier(key.identifier)
            }
            #if DEBUG
            // The guest TUI submits the composer with the send key, which the
            // software keyboard cannot send. A fixture run offers it on the
            // screen, so the test can drive one turn reproducibly.
            if controller.isFixtureRun {
                Button {
                    controller.sendInput(GuestTerminalKey.submit)
                } label: {
                    Text(Self.submitLabel)
                        .font(.caption.monospaced())
                        .frame(minWidth: 26)
                        .padding(.vertical, 2)
                        .background(Color.white.opacity(0.12))
                        .foregroundStyle(Color.white)
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("guest-key-submit")
            }
            #endif
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
    }

    /// The label of the interrupt control.
    private static let interruptLabel = "ctrl-c"
    /// The label of the send control.
    private static let submitLabel = "ctrl-s"

    /// The keys the host offers, in the order they are shown.
    private static let controlKeys: [ControlKey] = [
        ControlKey(identifier: "guest-key-escape", label: "esc", sequence: GuestTerminalKey.escape),
        ControlKey(identifier: "guest-key-tab", label: "tab", sequence: GuestTerminalKey.tab),
        ControlKey(identifier: "guest-key-left", label: "←", sequence: GuestTerminalKey.left),
        ControlKey(identifier: "guest-key-up", label: "↑", sequence: GuestTerminalKey.up),
        ControlKey(identifier: "guest-key-down", label: "↓", sequence: GuestTerminalKey.down),
        ControlKey(identifier: "guest-key-right", label: "→", sequence: GuestTerminalKey.right),
        ControlKey(identifier: "guest-key-interrupt", label: interruptLabel,
                   sequence: GuestTerminalKey.interrupt),
    ]

    /// One of the keys the host offers.
    private struct ControlKey: Identifiable {
        let identifier: String
        let label: String
        let sequence: String

        var id: String { identifier }
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
            Text(controller.state.summary)
                .font(.caption2.monospaced())
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("guest-fixture-state")
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
