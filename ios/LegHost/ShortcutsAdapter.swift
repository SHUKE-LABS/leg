import Foundation
import UIKit

/// The callback outcome from Apple's Shortcuts x-callback URL.
private enum ShortcutCallback {
    case success(String?)
    case cancelled
    case error(String?)
}

/// A native adapter for the `shortcuts.invoke` bridge capability.
///
/// The `BridgeListener` owns the transport and the approval ledger. This
/// adapter owns only the host-side registration lookup, confirmation prompt,
/// public Shortcuts URL, and callback lifetime. In particular, it never keeps
/// an approval token: a token is valid only in the listener that issued it.
@MainActor
final class ShortcutsAdapter: BridgeCapabilityAdapter {
    static let capability = "shortcuts.invoke"
    static let callbackScheme = "legshortcuts"
    static let maximumInputBytes = BridgeLimits.maxInputBytes
    static let maximumResultBytes = 32 * 1024
    // The Rust bridge rejects review values larger than 8 KiB. Keep the
    // human-facing fragment well below that ceiling because JSON encoding
    // escapes quotes, backslashes, and control characters again.
    static let maximumReviewSummaryBytes = 2 * 1024
    static let maximumReviewBytes = BridgeLimits.maxReviewBytes

    private let registry: ShortcutRegistrationStore
    private let confirmation: (String, String) async -> Bool
    private let opener: (URL) async -> Bool
    private var callbacks: [String: AsyncStream<ShortcutCallback>.Continuation] = [:]
    private var timeoutTasks: [String: Task<Void, Never>] = [:]

    init(registry: ShortcutRegistrationStore,
         confirmation: @escaping (String, String) async -> Bool = { name, summary in
             await ShortcutConfirmation.confirm(name: name, summary: summary)
         },
         opener: @escaping (URL) async -> Bool = { url in
             await withCheckedContinuation { continuation in
                 UIApplication.shared.open(url, options: [:]) { opened in
                     continuation.resume(returning: opened)
                 }
             }
         }) {
        self.registry = registry
        self.confirmation = confirmation
        self.opener = opener
    }

    /// The protocol methods are nonisolated wrappers so the listener can call
    /// this UI-owned adapter from its worker task. The actual work hops to the
    /// main actor before it touches registration state or UIKit.
    nonisolated func preview(_ request: BridgeCapabilityRequest,
                             context: BridgeOperationContext) async throws -> BridgeJSON {
        try await previewOnMain(request, context: context)
    }

    nonisolated func perform(_ request: BridgeCapabilityRequest,
                             context: BridgeOperationContext) async throws -> BridgeJSON {
        try await performOnMain(request, context: context)
    }

    private func previewOnMain(_ request: BridgeCapabilityRequest,
                               context: BridgeOperationContext) async throws -> BridgeJSON {
        guard case .shortcutsInvoke(let input) = request else {
            throw BridgeFailure.protocolViolation("\(request.capability) runs without a Shortcut request")
        }
        let registration = try registration(for: input)
        let validation = try validate(input.input, for: registration.inputKind)
        let summary = inputSummary(validation: validation)
        guard await confirmation(registration.name, summary) else {
            throw BridgeFailure.denied("user_refused", "the user did not approve running the registered Shortcut")
        }
        if Task.isCancelled { throw BridgeFailure.cancelled }

        let review = BridgeJSON.object([
            "shortcut": .string(registration.name),
            "input": .string(summary),
            "confirmation": .string("The user approved this registered Shortcut run."),
        ])
        guard BridgeCanonicalJSON.encode(review).count <= Self.maximumReviewBytes else {
            throw BridgeFailure.error("review_too_large", "the Shortcut confirmation summary is too large")
        }
        return review
    }

    private func performOnMain(_ request: BridgeCapabilityRequest,
                               context: BridgeOperationContext) async throws -> BridgeJSON {
        guard case .shortcutsInvoke(let input) = request else {
            throw BridgeFailure.protocolViolation("\(request.capability) runs without a Shortcut request")
        }
        let registration = try registration(for: input)
        let validation = try validate(input.input, for: registration.inputKind)
        if Task.isCancelled { throw BridgeFailure.cancelled }
        return try await invoke(registration: registration, input: validation,
                                deadline: context.deadline)
    }

    private func registration(for input: ShortcutsInvokeInput) throws -> ShortcutRegistration {
        guard let registration = registry.registration(named: input.name) else {
            throw BridgeFailure.denied(
                "shortcut_not_registered",
                "the Shortcut name is not registered in LegHost")
        }
        return registration
    }

    /// Handles a callback URL registered in the app's Info.plist.
    func handleCallback(_ url: URL) {
        guard url.scheme == Self.callbackScheme else { return }
        let components = URLComponents(url: url, resolvingAgainstBaseURL: false)
        guard let callbackState = components?.queryItems?.first(where: { $0.name == "state" })?.value,
              callbacks[callbackState] != nil else { return }
        let callback: ShortcutCallback
        switch url.host {
        case "success":
            callback = .success(
                components?.queryItems?.first(where: { $0.name == "result" })?.value)
        case "cancel":
            callback = .cancelled
        case "error":
            callback = .error(
                components?.queryItems?.first(where: { $0.name == "errorMessage" })?.value
                    ?? components?.queryItems?.first(where: { $0.name == "error" })?.value)
        default:
            callback = .error("unknown callback")
        }
        finishCallback(callbackState, result: callback)
    }

    /// Cancels callbacks when the guest session is torn down or interrupted.
    func cancelAll() {
        ShortcutConfirmation.cancel()
        for callbackState in Array(callbacks.keys) {
            finishCallback(callbackState, result: .cancelled)
        }
        timeoutTasks.values.forEach { $0.cancel() }
        timeoutTasks.removeAll()
    }

    /// Builds the only URL this adapter can open. The name is supplied by a
    /// registration lookup before this method is called.
    nonisolated static func shortcutURL(name: String,
                                        input: String,
                                        callbackState: String) -> URL? {
        shortcutURL(name: name, text: input, callbackState: callbackState)
    }

    nonisolated static func shortcutURL(name: String,
                                        text: String,
                                        callbackState: String) -> URL? {
        var components = URLComponents()
        components.scheme = "shortcuts"
        components.host = "x-callback-url"
        components.path = "/run-shortcut"
        components.queryItems = [
            URLQueryItem(name: "name", value: name),
            URLQueryItem(name: "input", value: "text"),
            URLQueryItem(name: "text", value: text),
            URLQueryItem(name: "x-success",
                         value: "\(callbackScheme)://success?state=\(callbackState)"),
            URLQueryItem(name: "x-cancel",
                         value: "\(callbackScheme)://cancel?state=\(callbackState)"),
            URLQueryItem(name: "x-error",
                         value: "\(callbackScheme)://error?state=\(callbackState)"),
        ]
        return components.url
    }

    private enum ValidatedInput {
        case text(String)
        case json(BridgeJSON)
    }

    private func validate(_ input: BridgeJSON,
                          for kind: ShortcutInputKind) throws -> ValidatedInput {
        guard case .object(let members) = input else {
            throw BridgeFailure.invalidInput("the Shortcut input must be an object")
        }
        var fields: [String: BridgeJSON] = [:]
        for member in members {
            guard fields.updateValue(member.value, forKey: member.key) == nil else {
                throw BridgeFailure.invalidInput("the Shortcut input carries a duplicate field")
            }
        }
        switch kind {
        case .text:
            guard fields.count == 1,
                  let text = fields["text"]?.stringValue,
                  !text.isEmpty,
                  !text.unicodeScalars.contains(where: {
                      $0.properties.generalCategory == .control
                  }),
                  text.utf8.count <= Self.maximumInputBytes else {
                throw BridgeFailure.invalidInput(
                    "the input does not match the registered Shortcut input kind")
            }
            return .text(text)
        case .json:
            guard !fields.isEmpty,
                  BridgeCanonicalJSON.encode(input).count <= Self.maximumInputBytes else {
                throw BridgeFailure.invalidInput(
                    "the input does not match the registered Shortcut input kind")
            }
            return .json(input)
        }
    }

    private func inputSummary(validation: ValidatedInput) -> String {
        switch validation {
        case .text(let text):
            return "text input (\(text.count) characters)"
        case .json(let input):
            guard case .object(let members) = input else { return "JSON object" }
            let prefix = "JSON object with fields: "
            var summary = prefix
            for (index, key) in members.map(\.key).sorted().enumerated() {
                let separator = index == 0 ? "" : ", "
                let escapedKey = escapedReviewFragment(key,
                                                       maximumBytes: Self.maximumReviewSummaryBytes)
                let candidate = summary + separator + escapedKey
                if candidate.utf8.count + "…".utf8.count > Self.maximumReviewSummaryBytes {
                    return summary + "…"
                }
                summary = candidate
            }
            return summary
        }
    }

    /// Escapes untrusted JSON keys before putting them into a human-facing
    /// review string. Each escape is appended as a complete token so a byte
    /// bound cannot leave a dangling JSON escape sequence.
    private func escapedReviewFragment(_ value: String, maximumBytes: Int) -> String {
        var escaped = ""
        for scalar in value.unicodeScalars {
            let token: String
            switch scalar.value {
            case 0x08: token = "\\b"
            case 0x09: token = "\\t"
            case 0x0A: token = "\\n"
            case 0x0C: token = "\\f"
            case 0x0D: token = "\\r"
            case 0x00...0x1F: token = String(format: "\\u%04X", scalar.value)
            case 0x22: token = "\\\""
            case 0x5C: token = "\\\\"
            default: token = String(scalar)
            }
            if escaped.utf8.count + token.utf8.count + "…".utf8.count > maximumBytes {
                return escaped + "…"
            }
            escaped.append(contentsOf: token)
        }
        return escaped
    }

    private func invoke(registration: ShortcutRegistration,
                        input: ValidatedInput,
                        deadline: Date) async throws -> BridgeJSON {
        if Task.isCancelled { throw BridgeFailure.cancelled }
        let callbackState = UUID().uuidString
        let url: URL?
        switch input {
        case .text(let text):
            url = Self.shortcutURL(name: registration.name, text: text,
                                   callbackState: callbackState)
        case .json(let object):
            let json = String(decoding: BridgeCanonicalJSON.encode(object), as: UTF8.self)
            url = Self.shortcutURL(name: registration.name, text: json,
                                   callbackState: callbackState)
        }

        guard let url else {
            throw BridgeFailure.error(
                "invalid_shortcut_url",
                "the registered Shortcut URL could not be built")
        }
        let callbackStream = registerCallback(callbackState: callbackState,
                                              timeout: max(0, deadline.timeIntervalSinceNow))
        guard await opener(url) else {
            finishCallback(callbackState, result: .error("shortcuts_unavailable"))
            throw BridgeFailure.unavailable(
                "shortcuts_unavailable",
                "the Shortcuts app is not available on this device")
        }
        if Task.isCancelled {
            finishCallback(callbackState, result: .cancelled)
            throw BridgeFailure.cancelled
        }

        let callback = await waitForCallback(callbackStream, callbackState: callbackState)
        switch callback {
        case .cancelled:
            throw BridgeFailure.cancelled
        case .error(let detail):
            let normalized = (detail ?? "").lowercased()
            if normalized == "timeout" {
                throw BridgeFailure.error(
                    "timeout",
                    "the Shortcut did not return before the deadline")
            }
            if normalized.contains("not found") || normalized.contains("could not be found")
                || normalized.contains("unknown shortcut") || normalized.contains("does not exist") {
                throw BridgeFailure.unavailable(
                    "shortcut_not_found",
                    "the registered Shortcut could not be found")
            }
            if normalized == "shortcuts_unavailable" {
                throw BridgeFailure.unavailable(
                    "shortcuts_unavailable",
                    "the Shortcuts app is not available on this device")
            }
            throw BridgeFailure.error(
                "shortcut_failed",
                "the Shortcut reported an error")
        case .success(let result):
            guard let result, !result.isEmpty else {
                throw BridgeFailure.error(
                    "missing_result",
                    "the Shortcut completed without a result")
            }
            guard result.utf8.count <= Self.maximumResultBytes else {
                throw BridgeFailure.error(
                    "result_too_large",
                    "the Shortcut result is larger than the 32 KiB limit")
            }
            if let data = Data(base64Encoded: result),
               let json = try? BridgeCanonicalJSON.parse(data) {
                return json
            }
            if let data = result.data(using: .utf8),
               let json = try? BridgeCanonicalJSON.parse(data) {
                return json
            }
            return .object([
                "kind": .string("text"),
                "value": .string(result),
            ])
        }
    }

    private func registerCallback(callbackState: String,
                                  timeout: TimeInterval) -> AsyncStream<ShortcutCallback> {
        let stream = AsyncStream<ShortcutCallback> { continuation in
            callbacks[callbackState] = continuation
        }
        let nanoseconds = UInt64(max(0, min(timeout, 30)) * 1_000_000_000)
        timeoutTasks[callbackState] = Task { [weak self] in
            if nanoseconds > 0 {
                try? await Task.sleep(nanoseconds: nanoseconds)
            }
            guard !Task.isCancelled else { return }
            await MainActor.run { self?.finishCallback(callbackState, result: .error("timeout")) }
        }
        return stream
    }

    private func waitForCallback(_ stream: AsyncStream<ShortcutCallback>,
                                 callbackState: String) async -> ShortcutCallback {
        await withTaskCancellationHandler {
            var iterator = stream.makeAsyncIterator()
            return await iterator.next() ?? .cancelled
        } onCancel: {
            Task { @MainActor [weak self] in
                self?.finishCallback(callbackState, result: .cancelled)
            }
        }
    }

    private func finishCallback(_ callbackState: String, result: ShortcutCallback) {
        timeoutTasks.removeValue(forKey: callbackState)?.cancel()
        guard let continuation = callbacks.removeValue(forKey: callbackState) else { return }
        continuation.yield(result)
        continuation.finish()
    }
}

/// Presents the explicit run confirmation without putting the confirmation
/// decision into the guest or the model.
@MainActor
private enum ShortcutConfirmation {
    private static var pending: (alert: UIAlertController,
                                  continuation: CheckedContinuation<Bool, Never>)?

    static func confirm(name: String, summary: String) async -> Bool {
        guard let presenter = topViewController() else { return false }
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                let alert = UIAlertController(title: "Run Shortcut?",
                                              message: "\(name)\n\(summary)",
                                              preferredStyle: .alert)
                Self.pending = (alert, continuation)
                alert.addAction(UIAlertAction(title: "Deny", style: .cancel) { _ in
                    Task { @MainActor in Self.finish(false) }
                })
                alert.addAction(UIAlertAction(title: "Allow", style: .default) { _ in
                    Task { @MainActor in Self.finish(true) }
                })
                presenter.present(alert, animated: true)
            }
        } onCancel: {
            Task { @MainActor in Self.cancel() }
        }
    }

    static func cancel() {
        guard let pending else { return }
        Self.pending = nil
        pending.alert.dismiss(animated: true)
        pending.continuation.resume(returning: false)
    }

    private static func finish(_ result: Bool) {
        guard let pending else { return }
        Self.pending = nil
        pending.continuation.resume(returning: result)
    }

    private static func topViewController() -> UIViewController? {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let window = scenes.flatMap(\.windows).first { $0.isKeyWindow }
        var controller = window?.rootViewController
        while let presented = controller?.presentedViewController { controller = presented }
        return controller
    }
}

// End of file
