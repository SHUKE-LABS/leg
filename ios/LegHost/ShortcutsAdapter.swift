import Foundation
import UIKit
import UniformTypeIdentifiers

/// A bounded JSON value used by the native side of the capability bridge.
///
/// The Rust bridge already validates the outer request. Keeping the native
/// envelope typed prevents the adapter from falling back to stringly-typed
/// URL construction or accepting a request-selected endpoint.
enum BridgeJSONValue: Codable, Equatable {
    case object([String: BridgeJSONValue])
    case array([BridgeJSONValue])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    init(from decoder: Decoder) throws {
        if let keyed = try? decoder.container(keyedBy: DynamicCodingKey.self) {
            var object: [String: BridgeJSONValue] = [:]
            for key in keyed.allKeys {
                object[key.stringValue] = try keyed.decode(BridgeJSONValue.self, forKey: key)
            }
            self = .object(object)
            return
        }
        if var unkeyed = try? decoder.unkeyedContainer() {
            var values: [BridgeJSONValue] = []
            while !unkeyed.isAtEnd {
                values.append(try unkeyed.decode(BridgeJSONValue.self))
            }
            self = .array(values)
            return
        }
        let value = try decoder.singleValueContainer()
        if value.decodeNil() {
            self = .null
        } else if let bool = try? value.decode(Bool.self) {
            self = .bool(bool)
        } else if let number = try? value.decode(Double.self) {
            self = .number(number)
        } else if let string = try? value.decode(String.self) {
            self = .string(string)
        } else {
            throw DecodingError.dataCorruptedError(in: value,
                                                   debugDescription: "unsupported JSON value")
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .object(let object):
            var keyed = encoder.container(keyedBy: DynamicCodingKey.self)
            for (key, value) in object {
                try keyed.encode(value, forKey: DynamicCodingKey(stringValue: key))
            }
        case .array(let values):
            var unkeyed = encoder.unkeyedContainer()
            for value in values { try unkeyed.encode(value) }
        case .string(let value):
            try value.encode(to: encoder)
        case .number(let value):
            try value.encode(to: encoder)
        case .bool(let value):
            try value.encode(to: encoder)
        case .null:
            var single = encoder.singleValueContainer()
            try single.encodeNil()
        }
    }

    var objectValue: [String: BridgeJSONValue]? {
        guard case .object(let object) = self else { return nil }
        return object
    }

    var stringValue: String? {
        guard case .string(let value) = self else { return nil }
        return value
    }

    var jsonData: Data? {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return try? encoder.encode(self)
    }

    static func decode(_ data: Data) -> BridgeJSONValue? {
        try? JSONDecoder().decode(BridgeJSONValue.self, from: data)
    }

}

private struct DynamicCodingKey: CodingKey {
    let stringValue: String
    let intValue: Int? = nil

    init(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}

/// The request envelope emitted by the Rust `SocketHostAdapter`.
struct ShortcutBridgeRequest: Codable, Equatable {
    let version: Int
    let requestID: String
    let nonce: String
    let capability: String
    let input: [String: BridgeJSONValue]
    let inputSHA256: String
    let approvalToken: String?
    let timeoutMS: Int

    enum CodingKeys: String, CodingKey {
        case version
        case requestID = "request_id"
        case nonce
        case capability
        case input
        case inputSHA256 = "input_sha256"
        case approvalToken = "approval_token"
        case timeoutMS = "timeout_ms"
    }
}

struct ShortcutBridgeError: Codable, Equatable {
    let reason: String
    let message: String
    let remediation: String
}

/// The answer envelope expected by the Rust bridge.
struct ShortcutBridgeReply: Codable, Equatable {
    let version: Int
    let requestID: String
    let nonce: String
    let status: String
    let result: BridgeJSONValue?
    let error: ShortcutBridgeError?

    enum CodingKeys: String, CodingKey {
        case version
        case requestID = "request_id"
        case nonce
        case status
        case result
        case error
    }

    static func approval(request: ShortcutBridgeRequest,
                         token: String,
                         review: BridgeJSONValue) -> ShortcutBridgeReply {
        ShortcutBridgeReply(version: 1, requestID: request.requestID, nonce: request.nonce,
                            status: "approval_required",
                            result: .object([
                                "approval_token": .string(token),
                                "review": review,
                            ]), error: nil)
    }

    static func ok(request: ShortcutBridgeRequest, result: BridgeJSONValue) -> ShortcutBridgeReply {
        ShortcutBridgeReply(version: 1, requestID: request.requestID, nonce: request.nonce,
                            status: "ok", result: result, error: nil)
    }

    static func failure(request: ShortcutBridgeRequest,
                        status: String,
                        reason: String,
                        message: String,
                        remediation: String) -> ShortcutBridgeReply {
        ShortcutBridgeReply(version: 1, requestID: request.requestID, nonce: request.nonce,
                            status: status, result: nil,
                            error: ShortcutBridgeError(reason: reason, message: message,
                                                      remediation: remediation))
    }

    var data: Data? {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return try? encoder.encode(self)
    }
}

/// The callback outcome from Apple's Shortcuts x-callback URL.
private enum ShortcutCallback {
    case success(String?)
    case cancelled
    case error(String?)
}

/// A native adapter for the `shortcuts.invoke` bridge capability.
@MainActor
final class ShortcutsAdapter {
    static let capability = "shortcuts.invoke"
    static let callbackScheme = "legshortcuts"
    static let guestSocketName = "/tmp/leg-host-bridge.sock"
    static let maximumInputBytes = 32 * 1024
    static let maximumResultBytes = 32 * 1024
    static let maximumFileBytes: Int64 = 16 * 1024 * 1024
    static let approvalLifetime: TimeInterval = 10 * 60

    private struct Approval {
        let token: String
        let requestID: String
        let name: String
        let inputSHA256: String
        let registration: ShortcutRegistration
        let selectedFile: URL?
        let expiresAt: Date
        var used = false
    }

    private let registry: ShortcutRegistrationStore
    private let confirmation: (String, String) async -> Bool
    private let opener: (URL) async -> Bool
    private var approvals: [String: Approval] = [:]
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

    /// Handles one host request. The Rust bridge performs the outer protocol
    /// validation; this layer adds the registered-name and input-kind checks.
    func handle(_ request: ShortcutBridgeRequest) async -> ShortcutBridgeReply {
        guard request.version == 1, request.capability == Self.capability else {
            return .failure(request: request, status: "error", reason: "unknown_capability",
                            message: "the iOS host serves only shortcuts.invoke",
                            remediation: "send a shortcuts.invoke request")
        }
        guard let name = request.input["name"]?.stringValue else {
            return .failure(request: request, status: "error", reason: "missing_field",
                            message: "shortcuts.invoke needs a Shortcut name",
                            remediation: "send the exact registered Shortcut name")
        }
        guard let registration = registry.registration(named: name) else {
            return .failure(request: request, status: "denied", reason: "shortcut_not_registered",
                            message: "the Shortcut name is not registered in LegHost",
                            remediation: "open Shortcuts settings, register the exact existing name, and retry")
        }

        let nestedInput = request.input["input"]?.objectValue
        guard let validation = validate(input: nestedInput, for: registration.inputKind) else {
            return .failure(request: request, status: "error", reason: "invalid_input",
                            message: "the input does not match the registered Shortcut input kind",
                            remediation: "send the bounded text, file, or JSON input the Shortcut accepts")
        }

        if request.approvalToken == nil {
            let selectedFile: URL?
            if registration.inputKind == .file {
                selectedFile = await ShortcutDocumentPicker.pick()
                guard let selectedFile else {
                    return .failure(request: request, status: "cancelled", reason: "cancelled",
                                    message: "the file selection was cancelled",
                                    remediation: "choose a file and retry the Shortcut")
                }
            } else {
                selectedFile = nil
            }
            let summary = inputSummary(validation: validation, file: selectedFile)
            let confirmed = await confirmation(name, summary)
            if Task.isCancelled {
                selectedFile?.stopAccessingSecurityScopedResource()
                return .failure(request: request, status: "cancelled", reason: "cancelled",
                                message: "the Shortcut request was cancelled before approval",
                                remediation: "retry the Shortcut if it is still wanted")
            }
            guard confirmed else {
                selectedFile?.stopAccessingSecurityScopedResource()
                return .failure(request: request, status: "denied", reason: "user_refused",
                                message: "the user did not approve running the registered Shortcut",
                                remediation: "approve the Shortcut run or retry later")
            }
            let token = UUID().uuidString
            approvals[token] = Approval(token: token, requestID: request.requestID, name: name,
                                        inputSHA256: request.inputSHA256, registration: registration,
                                        selectedFile: selectedFile,
                                        expiresAt: Date().addingTimeInterval(Self.approvalLifetime))
            let review = BridgeJSONValue.object([
                "shortcut": .string(name),
                "input": .string(summary),
                "confirmation": .string("The user approved this registered Shortcut run."),
            ])
            return .approval(request: request, token: token, review: review)
        }

        guard let token = request.approvalToken, var approval = approvals[token] else {
            return .failure(request: request, status: "denied", reason: "approval_unknown",
                            message: "the approval token is not known to this host",
                            remediation: "ask the host for a fresh confirmation")
        }
        guard approval.expiresAt > Date() else {
            approvals.removeValue(forKey: token)
            approval.selectedFile?.stopAccessingSecurityScopedResource()
            return .failure(request: request, status: "denied", reason: "approval_expired",
                            message: "the Shortcut approval has expired",
                            remediation: "ask for a fresh confirmation")
        }
        guard !approval.used else {
            return .failure(request: request, status: "denied", reason: "approval_used",
                            message: "the Shortcut approval was already used",
                            remediation: "ask for a fresh confirmation")
        }
        guard approval.name == name && approval.inputSHA256 == request.inputSHA256 else {
            return .failure(request: request, status: "denied", reason: "approval_mismatch",
                            message: "the approval was for another Shortcut input",
                            remediation: "send the same name and input that the user approved")
        }
        if Task.isCancelled {
            approvals.removeValue(forKey: token)?.selectedFile?.stopAccessingSecurityScopedResource()
            return .failure(request: request, status: "cancelled", reason: "cancelled",
                            message: "the Shortcut request was cancelled before dispatch",
                            remediation: "retry the Shortcut if it is still wanted")
        }

        approval.used = true
        approvals[token] = approval
        defer {
            approvals.removeValue(forKey: token)?.selectedFile?.stopAccessingSecurityScopedResource()
        }
        return await invoke(request: request, registration: approval.registration,
                            input: validation, selectedFile: approval.selectedFile)
    }

    /// Handles a callback URL registered in the app's Info.plist.
    func handleCallback(_ url: URL) {
        guard url.scheme == Self.callbackScheme else { return }
        let components = URLComponents(url: url, resolvingAgainstBaseURL: false)
        guard let requestID = components?.queryItems?.first(where: { $0.name == "request_id" })?.value,
              callbacks[requestID] != nil else { return }
        let callback: ShortcutCallback
        switch url.host {
        case "success":
            callback = .success(
                components?.queryItems?.first(where: { $0.name == "result" })?.value)
        case "cancel":
            callback = .cancelled
        case "error":
            callback = .error(
                components?.queryItems?.first(where: { $0.name == "error" })?.value)
        default:
            callback = .error("unknown callback")
        }
        finishCallback(requestID, result: callback)
    }

    /// Cancels callbacks when the guest session is torn down or interrupted.
    func cancelAll() {
        ShortcutConfirmation.cancel()
        ShortcutDocumentPicker.cancel()
        for requestID in Array(callbacks.keys) {
            finishCallback(requestID, result: .cancelled)
        }
        timeoutTasks.values.forEach { $0.cancel() }
        timeoutTasks.removeAll()
        for approval in approvals.values {
            approval.selectedFile?.stopAccessingSecurityScopedResource()
        }
        approvals.removeAll()
    }

    /// Builds the only URL this adapter can open. The name is supplied by a
    /// registration lookup before this method is called.
    nonisolated static func shortcutURL(name: String,
                                        input: String,
                                        requestID: String) -> URL? {
        var components = URLComponents()
        components.scheme = "shortcuts"
        components.host = "run-shortcut"
        components.queryItems = [
            URLQueryItem(name: "name", value: name),
            URLQueryItem(name: "input", value: input),
            URLQueryItem(name: "x-success",
                         value: "\(callbackScheme)://success?request_id=\(requestID)"),
            URLQueryItem(name: "x-cancel",
                         value: "\(callbackScheme)://cancel?request_id=\(requestID)"),
            URLQueryItem(name: "x-error",
                         value: "\(callbackScheme)://error?request_id=\(requestID)"),
        ]
        return components.url
    }

    private enum ValidatedInput {
        case text(String)
        case file
        case json([String: BridgeJSONValue])
    }

    private func validate(input: [String: BridgeJSONValue]?,
                          for kind: ShortcutInputKind) -> ValidatedInput? {
        switch kind {
        case .text:
            guard let input, input.count == 1, let text = input["text"]?.stringValue,
                  !text.isEmpty, !text.unicodeScalars.contains(where: { $0.properties.isControl }),
                  text.utf8.count <= Self.maximumInputBytes else { return nil }
            return .text(text)
        case .file:
            // The file is selected by the host's document picker. A guest
            // path is never accepted as a file input.
            guard input == nil || input?.isEmpty == true else { return nil }
            return .file
        case .json:
            guard let input, !input.isEmpty, let data = BridgeJSONValue.object(input).jsonData,
                  data.count <= Self.maximumInputBytes else { return nil }
            return .json(input)
        }
    }

    private func inputSummary(validation: ValidatedInput, file: URL?) -> String {
        switch validation {
        case .text(let text):
            return "text input (\(text.count) characters)"
        case .file:
            let name = file?.lastPathComponent ?? "selected file"
            var size: Int?
            if let file,
               let values = try? file.resourceValues(forKeys: [.fileSizeKey]) {
                size = values.fileSize
            }
            if let size { return "file \(name) (\(size) bytes)" }
            return "file \(name)"
        case .json(let object):
            let keys = object.keys.sorted().joined(separator: ", ")
            return "JSON object with fields: \(keys)"
        }
    }

    private func invoke(request: ShortcutBridgeRequest,
                        registration: ShortcutRegistration,
                        input: ValidatedInput,
                        selectedFile: URL?) async -> ShortcutBridgeReply {
        var fileCopy: URL?
        defer {
            if let fileCopy { try? FileManager.default.removeItem(at: fileCopy) }
        }
        if Task.isCancelled {
            return .failure(request: request, status: "cancelled", reason: "cancelled",
                            message: "the Shortcut request was cancelled before dispatch",
                            remediation: "retry the Shortcut if it is still wanted")
        }
        let inputText: String
        switch input {
        case .text(let text):
            inputText = text
        case .json(let object):
            guard let data = BridgeJSONValue.object(object).jsonData,
                  let json = String(data: data, encoding: .utf8) else {
                return .failure(request: request, status: "error", reason: "invalid_input",
                                message: "the JSON input could not be encoded",
                                remediation: "send a smaller JSON object")
            }
            inputText = json
        case .file:
            guard let selectedFile else {
                return .failure(request: request, status: "error", reason: "missing_file",
                                message: "the host did not receive the selected file",
                                remediation: "choose a file and retry")
            }
            do {
                fileCopy = try copyFile(selectedFile)
                inputText = fileCopy?.absoluteString ?? ""
            } catch let error as ShortcutFileError {
                return .failure(request: request, status: "error", reason: error.reason,
                                message: error.message, remediation: "choose a smaller regular file and retry")
            } catch {
                return .failure(request: request, status: "error", reason: "file_copy_failed",
                                message: "the selected file could not be copied safely",
                                remediation: "check the file permission and retry")
            }
        }

        guard let url = Self.shortcutURL(name: registration.name, input: inputText,
                                         requestID: request.requestID) else {
            return .failure(request: request, status: "error", reason: "invalid_shortcut_url",
                            message: "the registered Shortcut URL could not be built",
                            remediation: "check the registered Shortcut name and input")
        }
        let callbackStream = registerCallback(requestID: request.requestID,
                                               timeoutMS: request.timeoutMS)
        guard await opener(url) else {
            finishCallback(request.requestID, result: .error("shortcuts_unavailable"))
            return .failure(request: request, status: "unavailable", reason: "shortcuts_unavailable",
                            message: "the Shortcuts app is not available on this device",
                            remediation: "install or enable Apple's Shortcuts app and retry")
        }
        if Task.isCancelled {
            finishCallback(request.requestID, result: .cancelled)
            return .failure(request: request, status: "cancelled", reason: "cancelled",
                            message: "the Shortcut request was cancelled after dispatch",
                            remediation: "check whether the Shortcut completed and retry if needed")
        }

        let callback = await waitForCallback(callbackStream, requestID: request.requestID)
        switch callback {
        case .cancelled:
            return .failure(request: request, status: "cancelled", reason: "cancelled",
                            message: "the Shortcut run was cancelled",
                            remediation: "retry the Shortcut if it is still wanted")
        case .error(let detail):
            let normalized = (detail ?? "").lowercased()
            if normalized == "timeout" {
                return .failure(request: request, status: "error", reason: "timeout",
                                message: "the Shortcut did not return before the deadline",
                                remediation: "make the Shortcut finish sooner and retry")
            }
            if normalized.contains("not found") || normalized.contains("unknown shortcut") {
                return .failure(request: request, status: "unavailable", reason: "shortcut_not_found",
                                message: "the registered Shortcut could not be found",
                                remediation: "check the exact name in the Shortcuts app")
            }
            return .failure(request: request, status: "error", reason: "shortcut_failed",
                            message: "the Shortcut reported an error",
                            remediation: "check the Shortcut and retry")
        case .success(let result):
            guard let result, !result.isEmpty else {
                return .failure(request: request, status: "error", reason: "missing_result",
                                message: "the Shortcut completed without a result",
                                remediation: "make the Shortcut return text or a JSON object")
            }
            guard result.utf8.count <= Self.maximumResultBytes else {
                return .failure(request: request, status: "error", reason: "result_too_large",
                                message: "the Shortcut result is larger than the 32 KiB limit",
                                remediation: "make the Shortcut return a smaller result")
            }
            if let data = Data(base64Encoded: result), let json = BridgeJSONValue.decode(data) {
                return .ok(request: request, result: json)
            }
            if let data = result.data(using: .utf8), let json = BridgeJSONValue.decode(data) {
                return .ok(request: request, result: json)
            }
            return .ok(request: request, result: .object([
                "kind": .string("text"),
                "value": .string(result),
            ]))
        }
    }

    private func copyFile(_ source: URL) throws -> URL {
        let values = try source.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
        guard values.isRegularFile == true else { throw ShortcutFileError.notRegular }
        guard let size = values.fileSize, Int64(size) <= Self.maximumFileBytes else {
            throw ShortcutFileError.tooLarge
        }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
            "LegHost-Shortcuts", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let destination = directory.appendingPathComponent(UUID().uuidString)
            .appendingPathExtension(source.pathExtension)
        do {
            try FileManager.default.copyItem(at: source, to: destination)
            let copied = try destination.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
            guard copied.isRegularFile == true else { throw ShortcutFileError.notRegular }
            guard let copiedSize = copied.fileSize,
                  Int64(copiedSize) <= Self.maximumFileBytes else {
                throw ShortcutFileError.tooLarge
            }
            return destination
        } catch {
            try? FileManager.default.removeItem(at: destination)
            throw error
        }
    }

    private func registerCallback(requestID: String,
                                  timeoutMS: Int) -> AsyncStream<ShortcutCallback> {
        let stream = AsyncStream<ShortcutCallback> { continuation in
            callbacks[requestID] = continuation
        }
        let timeout = max(100, min(timeoutMS, 30_000))
        timeoutTasks[requestID] = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(timeout) * 1_000_000)
            guard !Task.isCancelled else { return }
            await MainActor.run { self?.finishCallback(requestID, result: .error("timeout")) }
        }
        return stream
    }

    private func waitForCallback(_ stream: AsyncStream<ShortcutCallback>,
                                 requestID: String) async -> ShortcutCallback {
        await withTaskCancellationHandler {
            var iterator = stream.makeAsyncIterator()
            return await iterator.next() ?? .cancelled
        } onCancel: {
            Task { @MainActor [weak self] in
                self?.finishCallback(requestID, result: .cancelled)
            }
        }
    }

    private func finishCallback(_ requestID: String, result: ShortcutCallback) {
        timeoutTasks.removeValue(forKey: requestID)?.cancel()
        guard let continuation = callbacks.removeValue(forKey: requestID) else { return }
        continuation.yield(result)
        continuation.finish()
    }
}

private enum ShortcutFileError: Error {
    case notRegular
    case tooLarge

    var reason: String {
        switch self {
        case .notRegular: return "invalid_file"
        case .tooLarge: return "file_too_large"
        }
    }

    var message: String {
        switch self {
        case .notRegular: return "the selected input is not a regular file"
        case .tooLarge: return "the selected file is larger than the 16 MiB limit"
        }
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
/// The system file picker used only for a Shortcut's declared file input.
@MainActor
private final class ShortcutDocumentPicker: NSObject, UIDocumentPickerDelegate {
    private static var active: ShortcutDocumentPicker?
    private var continuation: CheckedContinuation<URL?, Never>?
    private weak var presentedController: UIDocumentPickerViewController?

    static func pick() async -> URL? {
        let picker = ShortcutDocumentPicker()
        active = picker
        return await picker.present()
    }

    static func cancel() {
        active?.cancel()
    }

    private func present() async -> URL? {
        guard let presenter = ShortcutConfirmation.topViewControllerForPicker() else {
            Self.active = nil
            return nil
        }
        return await withCheckedContinuation { continuation in
            self.continuation = continuation
            let picker = UIDocumentPickerViewController(forOpeningContentTypes: [UTType.item],
                                                         asCopy: false)
            self.presentedController = picker
            picker.delegate = self
            presenter.present(picker, animated: true)
        }
    }

    func documentPicker(_ controller: UIDocumentPickerViewController,
                        didPickDocumentsAt urls: [URL]) {
        controller.dismiss(animated: true)
        finish(urls.first)
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        controller.dismiss(animated: true)
        finish(nil)
    }

    private func cancel() {
        presentedController?.dismiss(animated: true)
        finish(nil)
    }

    private func finish(_ url: URL?) {
        presentedController = nil
        if let url { _ = url.startAccessingSecurityScopedResource() }
        continuation?.resume(returning: url)
        continuation = nil
        Self.active = nil
    }
}

private extension ShortcutConfirmation {
    static func topViewControllerForPicker() -> UIViewController? {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let window = scenes.flatMap(\.windows).first { $0.isKeyWindow }
        var controller = window?.rootViewController
        while let presented = controller?.presentedViewController { controller = presented }
        return controller
    }
}

// End of file
