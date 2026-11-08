import SwiftUI

/// The host-owned form for the provider configuration.
///
/// The form never prints the API key. It hands the draft to the controller,
/// which validates and writes it through the same Keychain store the guest
/// launch reads.
struct ProviderSetupView: View {
    @ObservedObject private var controller: GuestHostController
    private let required: Bool
    private let onDismiss: (() -> Void)?

    @State private var draft: ProviderConfigurationDraft
    @State private var fieldErrors: [ProviderConfigurationField: String] = [:]
    @State private var storeError = ""

    init(controller: GuestHostController,
         required: Bool,
         onDismiss: (() -> Void)? = nil) {
        self.controller = controller
        self.required = required
        self.onDismiss = onDismiss
        _draft = State(initialValue: controller.providerDraft)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("Provider setup")
                    .font(.largeTitle.bold())
                Text(required
                     ? "Add an Anthropic provider before the guest can start."
                     : "Replace or clear the provider used by the next launch.")
                    .font(.headline)
                if required && !controller.detail.isEmpty {
                    Text(controller.detail)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .accessibilityIdentifier("guest-status-detail")
                }
                Text(controller.state.summary)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("guest-fixture-state")
                Text("The API key is stored only in this device's protected Keychain item and is never shown in host diagnostics.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)

                providerField
                textField("API key", value: $draft.apiKey,
                          identifier: "provider-api-key", secure: true,
                          field: .apiKey)
                textField("Base URL", value: $draft.baseURL,
                          identifier: "provider-base-url", field: .baseURL)
                textField("Model", value: $draft.model,
                          identifier: "provider-model", field: .model)
                textField("Context window (tokens)", value: $draft.contextWindowTokens,
                          identifier: "provider-context-window", field: .contextWindowTokens)
                    .keyboardType(.numberPad)

                if !storeError.isEmpty {
                    Text(storeError)
                        .font(.footnote)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("provider-save-error")
                }

                HStack(spacing: 12) {
                    if onDismiss != nil {
                        Button("Cancel") { onDismiss?() }
                            .accessibilityIdentifier("provider-cancel")
                    }
                    Spacer()
                    Button("Clear") { clear() }
                        .accessibilityIdentifier("provider-clear")
                    Button("Save") { save() }
                        .buttonStyle(.borderedProminent)
                        .accessibilityIdentifier("provider-save")
                }
            }
            .padding()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    /// The only provider protocol this host exposes.
    private var providerField: some View {
        VStack(alignment: .leading, spacing: 6) {
            Picker("Provider", selection: $draft.provider) {
                Text("Anthropic Messages").tag(HostProvider.anthropic)
            }
            .accessibilityIdentifier("provider-choice")
            fieldError(.provider)
        }
    }

    /// A text or secure field with its field-level validation message.
    @ViewBuilder
    private func textField(_ title: String,
                           value: Binding<String>,
                           identifier: String,
                           secure: Bool = false,
                           field: ProviderConfigurationField) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            if secure {
                SecureField(title, text: value)
                    .accessibilityIdentifier(identifier)
            } else {
                TextField(title, text: value)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier(identifier)
            }
            fieldError(field)
        }
    }

    /// The message for one setup field, if validation failed there.
    @ViewBuilder
    private func fieldError(_ field: ProviderConfigurationField) -> some View {
        if let message = fieldErrors[field] {
            Text(message)
                .font(.footnote)
                .foregroundStyle(.red)
                .accessibilityIdentifier("provider-\(field.rawValue)-error")
        }
    }

    /// Validates and saves the draft without exposing its credential.
    private func save() {
        fieldErrors = [:]
        storeError = ""
        let validationErrors = ProviderConfigurationStore.validationErrors(draft)
        if !validationErrors.isEmpty {
            fieldErrors = Dictionary(uniqueKeysWithValues: validationErrors.map {
                ($0.field, $0.localizedDescription)
            })
            return
        }
        do {
            try controller.saveProviderConfiguration(draft)
            onDismiss?()
        } catch let error as ProviderConfigurationValidationError {
            fieldErrors[error.field] = error.localizedDescription
        } catch {
            storeError = error.localizedDescription
        }
    }

    /// Clears the persisted item and resets the form without logging its value.
    private func clear() {
        fieldErrors = [:]
        storeError = ""
        do {
            try controller.clearProviderConfiguration()
            draft = .empty
            onDismiss?()
        } catch {
            storeError = error.localizedDescription
        }
    }
}
