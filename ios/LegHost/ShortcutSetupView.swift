import SwiftUI

/// The host-owned registration form for existing iOS Shortcuts.
struct ShortcutSetupView: View {
    @ObservedObject private var controller: GuestHostController
    @ObservedObject private var registry: ShortcutRegistrationStore
    private let onDismiss: (() -> Void)?

    @State private var name = ""
    @State private var inputKind: ShortcutInputKind = .text
    @State private var error = ""

    init(controller: GuestHostController, onDismiss: (() -> Void)? = nil) {
        self.controller = controller
        self.registry = controller.shortcutRegistry
        self.onDismiss = onDismiss
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text("Register an existing Shortcut by its exact name. Leg never creates, edits, enables, or discovers personal automations.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                    TextField("Exact Shortcut name", text: $name)
                        .textInputAutocapitalization(.sentences)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("shortcut-name")
                    Picker("Accepted input", selection: $inputKind) {
                        ForEach(ShortcutInputKind.allCases) { kind in
                            Text(kind.title).tag(kind)
                        }
                    }
                    .accessibilityIdentifier("shortcut-input-kind")
                    Text(inputKind.explanation)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } header: {
                    Text("Register Shortcut")
                }

                if !error.isEmpty {
                    Text(error)
                        .foregroundStyle(.red)
                        .font(.footnote)
                        .accessibilityIdentifier("shortcut-save-error")
                }

                Section("Registered Shortcuts") {
                    if registry.registrations.isEmpty {
                        Text("None")
                            .foregroundStyle(.secondary)
                            .accessibilityIdentifier("shortcut-empty")
                    } else {
                        ForEach(registry.registrations) { registration in
                            HStack {
                                VStack(alignment: .leading, spacing: 3) {
                                    Text(registration.name)
                                    Text(registration.inputKind.title)
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button("Remove", role: .destructive) {
                                    remove(registration)
                                }
                                .accessibilityIdentifier("shortcut-remove-\(registration.name)")
                            }
                        }
                    }
                }
            }
            .navigationTitle("Shortcuts")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    if onDismiss != nil {
                        Button("Done") { onDismiss?() }
                            .accessibilityIdentifier("shortcut-done")
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") { save() }
                        .disabled(name.isEmpty)
                        .accessibilityIdentifier("shortcut-save")
                }
            }
        }
    }

    private func save() {
        error = ""
        do {
            try controller.saveShortcutRegistration(name: name, inputKind: inputKind)
            name = ""
        } catch {
            error = error.localizedDescription
        }
    }

    private func remove(_ registration: ShortcutRegistration) {
        do {
            try controller.removeShortcutRegistration(name: registration.name)
        } catch {
            error = error.localizedDescription
        }
    }
}
