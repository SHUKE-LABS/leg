import SwiftUI

/// Native iOS host scaffold.
///
/// The host embeds the pinned iSH revision vendored under `vendor/ish`; the
/// revision marker travels in the app bundle and is shown here. This slice
/// does not ingest the guest bundle, so no guest process is started.
@main
struct LegHostApp: App {
    var body: some Scene {
        WindowGroup {
            HostPlaceholderView()
        }
    }
}

struct HostPlaceholderView: View {
    private let revision = HostBundle.ishRevision ?? "unknown"

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("LegHost")
                .font(.largeTitle.bold())
            Text("Native iOS host scaffold")
                .font(.headline)
            Divider()
            LabeledContent("Vendored iSH revision", value: revision)
            LabeledContent("Guest runtime", value: "not started")
            Text("""
                The pinned iSH runtime is linked into this binary. This slice \
                does not ingest the #253 guest bundle, does not open a PTY, \
                and does not expose native Photos, Files or Shortcuts \
                capabilities.
                """)
                .font(.footnote)
                .foregroundStyle(.secondary)
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
