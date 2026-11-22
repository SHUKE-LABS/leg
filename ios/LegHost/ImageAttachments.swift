import Foundation
import PhotosUI
import UIKit
import UniformTypeIdentifiers

// MARK: - The contract

/// Where an image came from. The raw value is the source the guest notice
/// carries.
enum ImageAttachmentSource: String, CaseIterable {
    case photos
    case files
    case pasteboard

    /// The marker the preview and the guest draft show before the filename.
    var marker: String {
        switch self {
        case .photos: return "Photos"
        case .files: return "Files"
        case .pasteboard: return "Clipboard"
        }
    }
}

/// The image limits the guest enforces as well, in base64-encoded bytes.
struct ImageAttachmentLimits: Equatable {
    var maxEncodedBytesPerImage = 10_000_000
    var maxEncodedBytesPerTurn = 32_000_000

    static let shared = ImageAttachmentLimits()

    /// The base64 length of `byteCount` raw bytes.
    static func encodedLength(_ byteCount: Int) -> Int {
        (byteCount + 2) / 3 * 4
    }

    /// The largest raw image whose encoding fits the per-image limit.
    var maxBytesPerImage: Int { maxEncodedBytesPerImage / 4 * 3 }
}

/// An acquisition or a handoff that did not complete: a stable reason and a
/// message that says what to do next. Neither names a path or the bytes.
struct ImageAttachmentFailure: Error, Equatable {
    let reason: String
    let message: String

    static let cancelled = ImageAttachmentFailure(
        reason: "cancelled", message: "No image was attached.")
    static let pasteDenied = ImageAttachmentFailure(
        reason: "paste_denied",
        message: "LegHost may not read the clipboard. Allow Paste from Other Apps for LegHost in Settings, then paste again.")
    static let noPasteboardImage = ImageAttachmentFailure(
        reason: "no_image",
        message: "The clipboard holds no image. Copy an image first; text pastes through the keyboard.")
    static let photoUnreadable = ImageAttachmentFailure(
        reason: "unreadable",
        message: "The photo could not be read. If it is stored in iCloud, open it in Photos to download it, then pick it again.")
    static let accessRevoked = ImageAttachmentFailure(
        reason: "permission_revoked",
        message: "The Files provider no longer grants access to the picked file. Pick it again.")
    static let unreadable = ImageAttachmentFailure(
        reason: "unreadable",
        message: "The picked file could not be read. Pick it again or choose another file.")
    static let unsupportedType = ImageAttachmentFailure(
        reason: "unsupported_image",
        message: "Only JPEG, PNG, GIF, and WebP images can be attached.")
    static let typeMismatch = ImageAttachmentFailure(
        reason: "media_type_mismatch",
        message: "The image content does not match its declared type. Export it again as JPEG or PNG.")
    static let stagingUnavailable = ImageAttachmentFailure(
        reason: "staging_unavailable",
        message: "LegHost could not stage the image. Free some storage, then try again.")
    static let sessionEnded = ImageAttachmentFailure(
        reason: "session_ended",
        message: "The session the image was picked for has ended. Pick it again.")
    static let receiverUnavailable = ImageAttachmentFailure(
        reason: "receiver_unavailable",
        message: "The guest program cannot receive images. Update LegHost to a build whose guest supports images.")
    static let handoffFailed = ImageAttachmentFailure(
        reason: "handoff_failed",
        message: "The image could not be handed to the guest. Pick it again.")

    static func tooLarge(_ limits: ImageAttachmentLimits) -> ImageAttachmentFailure {
        ImageAttachmentFailure(
            reason: "image_too_large",
            message: "The image is over the \(limits.maxEncodedBytesPerImage / 1_000_000) MB per-image limit. "
                + "Pick a smaller image.")
    }

    static func turnFull(_ limits: ImageAttachmentLimits) -> ImageAttachmentFailure {
        ImageAttachmentFailure(
            reason: "turn_limit",
            message: "The waiting images would exceed the \(limits.maxEncodedBytesPerTurn / 1_000_000) MB "
                + "per-turn limit. Insert or remove one first.")
    }
}

/// What a source read: the bytes, the name the user knows the image by, and
/// the type the source declared, when it declared one.
struct ImageAttachmentCandidate {
    let data: Data
    let filename: String?
    let declaredMediaType: String?
}

/// The image types a turn can carry.
enum ImageMediaType {
    /// The supported types, in the order a source is asked for them.
    static let supported: [UTType] = [.png, .jpeg, .gif, .webP]

    /// The type the leading bytes of `data` prove, or nil.
    static func detect(_ data: Data) -> String? {
        let bytes = [UInt8](data.prefix(12))
        if bytes.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if bytes.starts(with: [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) { return "image/png" }
        if bytes.starts(with: Array("GIF87a".utf8)) || bytes.starts(with: Array("GIF89a".utf8)) {
            return "image/gif"
        }
        if bytes.count == 12, bytes.starts(with: Array("RIFF".utf8)), Array(bytes[8..<12]) == Array("WEBP".utf8) {
            return "image/webp"
        }
        return nil
    }

    /// The MIME type of a supported uniform type, or nil.
    static func of(_ type: UTType) -> String? {
        guard supported.contains(type) else { return nil }
        return type.preferredMIMEType
    }

    /// The file extension of a supported MIME type.
    static func fileExtension(_ mediaType: String) -> String {
        UTType(mimeType: mediaType)?.preferredFilenameExtension ?? "img"
    }

    /// A basename the preview and the guest notice may show: the last path
    /// component without control characters, bounded, with the extension of
    /// the type when it has none.
    static func safeFilename(_ raw: String?, mediaType: String) -> String {
        let ext = fileExtension(mediaType)
        let last = (raw ?? "")
            .split(whereSeparator: { $0 == "/" || $0 == "\\" || $0 == ":" })
            .last.map(String.init) ?? ""
        let scalars = last.precomposedStringWithCanonicalMapping.unicodeScalars
            .filter { $0.properties.generalCategory != .control && $0.properties.generalCategory != .format }
        var name = String(String.UnicodeScalarView(scalars))
        while name.utf8.count > 96 { name.removeLast() }
        name = name.trimmingCharacters(in: CharacterSet(charactersIn: " ."))
        guard !name.isEmpty else { return "image.\(ext)" }
        if (name as NSString).pathExtension.isEmpty {
            name += ".\(ext)"
        }
        return BridgeValidation.isSafeBasename(name) ? name : "image.\(ext)"
    }
}

// MARK: - Staging

/// One image staged for the running session. It holds metadata only; the
/// bytes stay in the staging directory of the session.
struct StagedImageAttachment: Identifiable, Equatable {
    /// The opaque handoff id, `ios-` and 32 hex digits.
    let id: String
    let generation: String
    let source: ImageAttachmentSource
    let filename: String
    let mediaType: String
    let byteCount: Int
    let sha256: String

    var encodedByteCount: Int { ImageAttachmentLimits.encodedLength(byteCount) }

    /// The source marker and the filename, as the guest draft shows them.
    var label: String { "\(source.marker) · \(filename)" }

    var sizeText: String {
        ByteCountFormatter.string(fromByteCount: Int64(byteCount), countStyle: .file)
    }
}

/// The images picked for the running session, staged in an app-owned
/// directory of that session generation.
///
/// Each session owns `<root>/<generation>`. A new session sweeps the whole
/// root, so what an earlier session, a crash, or a termination left behind is
/// gone before anything is staged; the end of a session removes it again.
final class ImageAttachmentStore {
    let root: URL
    let limits: ImageAttachmentLimits
    private(set) var generation: String?
    /// The staged images that were not inserted or removed yet, in order.
    private(set) var pending: [StagedImageAttachment] = []

    init(root: URL, limits: ImageAttachmentLimits = .shared) {
        self.root = root
        self.limits = limits
    }

    /// `Application Support/LegHost/attachments`.
    static func defaultRoot() throws -> URL {
        try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                    appropriateFor: nil, create: true)
            .appendingPathComponent("LegHost/attachments", isDirectory: true)
    }

    /// A fresh session generation: 16 hex digits.
    static func newGeneration() -> String {
        String(BridgeValidation.newGeneratedID().prefix(16))
    }

    /// Starts the staging of `generation`, sweeping every earlier one.
    func begin(generation: String) {
        try? FileManager.default.removeItem(at: root)
        pending = []
        self.generation = generation
    }

    /// Ends the staging of the session and removes its files.
    func end() {
        try? FileManager.default.removeItem(at: root)
        pending = []
        generation = nil
    }

    /// The staged file of `attachment`.
    func fileURL(for attachment: StagedImageAttachment) -> URL {
        root.appendingPathComponent(attachment.generation, isDirectory: true)
            .appendingPathComponent(attachment.id)
    }

    /// Validates `candidate` and stages it for `generation`. A failure leaves
    /// no file behind.
    func stage(_ candidate: ImageAttachmentCandidate, source: ImageAttachmentSource,
               generation: String) throws -> StagedImageAttachment {
        guard let current = self.generation, current == generation else {
            throw ImageAttachmentFailure.sessionEnded
        }
        guard let mediaType = ImageMediaType.detect(candidate.data) else {
            throw ImageAttachmentFailure.unsupportedType
        }
        if let declared = candidate.declaredMediaType, declared != mediaType {
            throw ImageAttachmentFailure.typeMismatch
        }
        let encoded = ImageAttachmentLimits.encodedLength(candidate.data.count)
        guard encoded <= limits.maxEncodedBytesPerImage else {
            throw ImageAttachmentFailure.tooLarge(limits)
        }
        guard pending.reduce(encoded, { $0 + $1.encodedByteCount }) <= limits.maxEncodedBytesPerTurn else {
            throw ImageAttachmentFailure.turnFull(limits)
        }
        let attachment = StagedImageAttachment(
            id: "ios-\(BridgeValidation.newGeneratedID())",
            generation: generation,
            source: source,
            filename: ImageMediaType.safeFilename(candidate.filename, mediaType: mediaType),
            mediaType: mediaType,
            byteCount: candidate.data.count,
            sha256: BridgeCanonicalJSON.sha256Hex(candidate.data))
        let destination = fileURL(for: attachment)
        let partial = destination.deletingLastPathComponent().appendingPathComponent(".\(attachment.id).partial")
        do {
            try FileManager.default.createDirectory(at: destination.deletingLastPathComponent(),
                                                    withIntermediateDirectories: true)
            try candidate.data.write(to: partial, options: .withoutOverwriting)
            try FileManager.default.moveItem(at: partial, to: destination)
        } catch {
            try? FileManager.default.removeItem(at: partial)
            try? FileManager.default.removeItem(at: destination)
            throw ImageAttachmentFailure.stagingUnavailable
        }
        pending.append(attachment)
        return attachment
    }

    /// Removes a staged image and its file.
    func remove(id: String) {
        guard let index = pending.firstIndex(where: { $0.id == id }) else { return }
        let attachment = pending.remove(at: index)
        try? FileManager.default.removeItem(at: fileURL(for: attachment))
    }

    /// Drops a staged image whose file was handed to the guest.
    func release(id: String) {
        pending.removeAll { $0.id == id }
    }
}

// MARK: - The guest handoff

/// The guest side of the handoff: one directory per session generation that
/// the guest receiver reads by opaque id.
///
/// The host moves a staged image into the directory, adopts it into the
/// fakefs, and writes a bracketed paste whose text is a notice: a fixed
/// prefix and JSON with the generation, the id, the source, the filename, the
/// MIME type, the byte count, and the SHA-256. Image bytes and host paths
/// never reach the terminal.
struct ImageAttachmentHandoff {
    static let environmentVariable = "LEG_HOST_ATTACHMENT_DIR"
    static let guestParent = "/tmp/leghost-attachments"
    /// The receiver writes it into the directory when it reads notices.
    static let readyMarker = ".receiver-v1"
    static let noticePrefix = "leghost-attachment/v1 "
    /// The bound the receiver puts on the notice text.
    static let maxNoticeBytes = 2048

    let generation: String
    /// `<active root>/data/tmp/leghost-attachments`.
    let hostParent: URL

    init(generation: String, activeRoot: URL) {
        self.generation = generation
        hostParent = activeRoot.appendingPathComponent("data/tmp/leghost-attachments", isDirectory: true)
    }

    var hostDirectory: URL { hostParent.appendingPathComponent(generation, isDirectory: true) }
    var guestDirectory: String { "\(Self.guestParent)/\(generation)" }
    /// The variable that names the directory to the guest program.
    var environment: String { "\(Self.environmentVariable)=\(guestDirectory)" }

    /// Removes the handoffs of every earlier session and makes the directory
    /// of this one visible to the guest.
    func prepare(adopt: (String) -> Bool) throws {
        try? FileManager.default.removeItem(at: hostParent)
        try FileManager.default.createDirectory(at: hostDirectory, withIntermediateDirectories: true)
        guard adopt(guestDirectory) else { throw ImageAttachmentFailure.handoffFailed }
    }

    /// Whether the guest program announced a receiver in the directory.
    var isReceiverReady: Bool {
        let marker = hostDirectory.appendingPathComponent(Self.readyMarker)
        let values = try? marker.resourceValues(forKeys: [.isRegularFileKey])
        return values?.isRegularFile == true
    }

    /// Moves the staged file into the directory, makes it visible to the
    /// guest, and returns the bytes of the notice. A failure leaves no
    /// handoff file.
    func publish(_ file: URL, attachment: StagedImageAttachment, adopt: (String) -> Bool) throws -> [UInt8] {
        guard attachment.generation == generation else { throw ImageAttachmentFailure.sessionEnded }
        let notice = Self.notice(for: attachment)
        let destination = hostDirectory.appendingPathComponent(attachment.id)
        do {
            try FileManager.default.moveItem(at: file, to: destination)
        } catch {
            throw ImageAttachmentFailure.handoffFailed
        }
        guard adopt("\(guestDirectory)/\(attachment.id)") else {
            discard(attachment)
            throw ImageAttachmentFailure.handoffFailed
        }
        return notice
    }

    /// Removes the handoff file of `attachment`.
    func discard(_ attachment: StagedImageAttachment) {
        try? FileManager.default.removeItem(at: hostDirectory.appendingPathComponent(attachment.id))
    }

    /// Removes the handoffs of the session.
    func clear() {
        try? FileManager.default.removeItem(at: hostParent)
    }

    /// The bracketed paste that announces `attachment`.
    static func notice(for attachment: StagedImageAttachment) -> [UInt8] {
        let fields: [String: Any] = [
            "generation": attachment.generation,
            "id": attachment.id,
            "source": attachment.source.rawValue,
            "label": attachment.filename,
            "media_type": attachment.mediaType,
            "byte_len": attachment.byteCount,
            "sha256": attachment.sha256,
        ]
        let json = (try? JSONSerialization.data(withJSONObject: fields,
                                                options: [.sortedKeys, .withoutEscapingSlashes])) ?? Data()
        let text = Array(noticePrefix.utf8) + [UInt8](json)
        precondition(text.count <= maxNoticeBytes, "the notice outgrew its bound")
        return Array("\u{1b}[200~".utf8) + text + Array("\u{1b}[201~".utf8)
    }
}

// MARK: - Sources

/// The places an image is read from. Picking is the consent: a source reads
/// only what the user picked or pasted.
@MainActor
protocol ImageAttachmentSources: AnyObject {
    func pickPhoto() async throws -> ImageAttachmentCandidate
    func pickFile() async throws -> ImageAttachmentCandidate
    func readPasteboard() throws -> ImageAttachmentCandidate
}

/// The sources of the running app: the Photos picker, the Files picker, and
/// the general pasteboard.
@MainActor
final class LiveImageAttachmentSources: ImageAttachmentSources {
    private let documents: BridgeDocumentPresenter
    private let pasteboard: UIPasteboard
    private let limits: ImageAttachmentLimits

    init(documents: BridgeDocumentPresenter, pasteboard: UIPasteboard = .general,
         limits: ImageAttachmentLimits = .shared) {
        self.documents = documents
        self.pasteboard = pasteboard
        self.limits = limits
    }

    func pickPhoto() async throws -> ImageAttachmentCandidate {
        let provider = try await PhotoPickerSession.pick()
        return try await ImageAttachmentPhotos.load(provider)
    }

    func pickFile() async throws -> ImageAttachmentCandidate {
        try await ImageAttachmentFiles.read(presenter: documents, limits: limits)
    }

    func readPasteboard() throws -> ImageAttachmentCandidate {
        try ImageAttachmentPasteboard.read(pasteboard)
    }
}

/// Reads a picked file inside the security scope its provider granted.
enum ImageAttachmentFiles {
    static func read(presenter: BridgeDocumentPresenter,
                     limits: ImageAttachmentLimits) async throws -> ImageAttachmentCandidate {
        let url: URL
        do {
            url = try await presenter.pickFileToImport()
        } catch let failure as BridgeFailure where failure.status == .cancelled {
            throw ImageAttachmentFailure.cancelled
        } catch {
            throw ImageAttachmentFailure.unreadable
        }
        guard presenter.startAccessing(url) else { throw ImageAttachmentFailure.accessRevoked }
        defer { presenter.stopAccessing(url) }
        guard let root = presenter.scopeRoot(for: url) else { throw ImageAttachmentFailure.unreadable }
        let maxBytes = limits.maxBytesPerImage
        // A provider may download the file first, so the read runs off the
        // main thread.
        let data: Data = try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(with: Result {
                    try BridgeFileSystem.coordinatedRead(url) { readURL in
                        let source = try BridgeFileSystem.openContained(readURL, root: root)
                        defer { close(source.descriptor) }
                        guard source.size <= Int64(maxBytes) else { throw ImageAttachmentFailure.tooLarge(limits) }
                        return try readAll(source.descriptor, maxBytes: maxBytes, limits: limits)
                    }
                }.mapError { $0 as? ImageAttachmentFailure ?? .unreadable })
            }
        }
        let declared = UTType(filenameExtension: url.pathExtension).flatMap(ImageMediaType.of)
        return ImageAttachmentCandidate(data: data, filename: url.lastPathComponent, declaredMediaType: declared)
    }

    /// Reads at most `maxBytes` bytes; a longer file is too large.
    private static func readAll(_ descriptor: Int32, maxBytes: Int,
                                limits: ImageAttachmentLimits) throws -> Data {
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: BridgeTransfer.chunkBytes)
        while true {
            let count = buffer.withUnsafeMutableBytes { Darwin.read(descriptor, $0.baseAddress, $0.count) }
            if count < 0 {
                if errno == EINTR { continue }
                throw ImageAttachmentFailure.unreadable
            }
            if count == 0 { return data }
            data.append(contentsOf: buffer[0..<count])
            guard data.count <= maxBytes else { throw ImageAttachmentFailure.tooLarge(limits) }
        }
    }
}

/// Reads an image off a pasteboard. Text is never read here: text pastes
/// through the keyboard as before.
enum ImageAttachmentPasteboard {
    static func read(_ pasteboard: UIPasteboard) throws -> ImageAttachmentCandidate {
        guard pasteboard.hasImages else { throw ImageAttachmentFailure.noPasteboardImage }
        for type in ImageMediaType.supported {
            if let data = pasteboard.data(forPasteboardType: type.identifier) {
                return ImageAttachmentCandidate(data: data, filename: "pasted-image",
                                                declaredMediaType: ImageMediaType.of(type))
            }
        }
        if let png = pasteboard.image?.pngData() {
            return ImageAttachmentCandidate(data: png, filename: "pasted-image", declaredMediaType: "image/png")
        }
        // The pasteboard has an image the user did not allow LegHost to read.
        throw ImageAttachmentFailure.pasteDenied
    }
}

/// Loads the image the Photos picker returned.
enum ImageAttachmentPhotos {
    static func load(_ provider: NSItemProvider) async throws -> ImageAttachmentCandidate {
        let name = provider.suggestedName
        for type in ImageMediaType.supported where provider.hasItemConformingToTypeIdentifier(type.identifier) {
            let data = try await loadData(provider, type: type)
            return ImageAttachmentCandidate(data: data, filename: name, declaredMediaType: ImageMediaType.of(type))
        }
        // A format a turn cannot carry, such as HEIC, is sent as JPEG.
        guard provider.canLoadObject(ofClass: UIImage.self) else { throw ImageAttachmentFailure.unsupportedType }
        let image: UIImage = try await withCheckedThrowingContinuation { continuation in
            _ = provider.loadObject(ofClass: UIImage.self) { object, _ in
                if let image = object as? UIImage {
                    continuation.resume(returning: image)
                } else {
                    continuation.resume(throwing: ImageAttachmentFailure.photoUnreadable)
                }
            }
        }
        guard let jpeg = image.jpegData(compressionQuality: 0.9) else {
            throw ImageAttachmentFailure.photoUnreadable
        }
        let base = name.map { ($0 as NSString).deletingPathExtension }
        return ImageAttachmentCandidate(data: jpeg, filename: base, declaredMediaType: "image/jpeg")
    }

    private static func loadData(_ provider: NSItemProvider, type: UTType) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            _ = provider.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, _ in
                if let data {
                    continuation.resume(returning: data)
                } else {
                    continuation.resume(throwing: ImageAttachmentFailure.photoUnreadable)
                }
            }
        }
    }
}

/// One Photos picker on screen and the request waiting for it.
///
/// The picker runs out of process and returns only what the user picked, so
/// it needs no photo library permission.
@MainActor
private final class PhotoPickerSession: NSObject, PHPickerViewControllerDelegate {
    private var continuation: CheckedContinuation<NSItemProvider, Error>?
    /// The session keeps itself alive while its picker is on screen.
    private var retainedSelf: PhotoPickerSession?

    static func pick() async throws -> NSItemProvider {
        var configuration = PHPickerConfiguration()
        configuration.filter = .images
        configuration.selectionLimit = 1
        configuration.preferredAssetRepresentationMode = .compatible
        let picker = PHPickerViewController(configuration: configuration)
        let session = PhotoPickerSession()
        return try await withCheckedThrowingContinuation { continuation in
            session.start(picker, continuation)
        }
    }

    private func start(_ picker: PHPickerViewController,
                       _ continuation: CheckedContinuation<NSItemProvider, Error>) {
        guard let presenter = Self.topViewController() else {
            continuation.resume(throwing: ImageAttachmentFailure.photoUnreadable)
            return
        }
        self.continuation = continuation
        retainedSelf = self
        picker.delegate = self
        // Closing goes through Cancel, which reports back; a swipe may not.
        picker.isModalInPresentation = true
        presenter.present(picker, animated: true)
    }

    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        guard let continuation else { return }
        self.continuation = nil
        retainedSelf = nil
        if let provider = results.first?.itemProvider {
            continuation.resume(returning: provider)
        } else {
            continuation.resume(throwing: ImageAttachmentFailure.cancelled)
        }
    }

    private static func topViewController() -> UIViewController? {
        let window = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap(\.windows)
            .first { $0.isKeyWindow }
        var top = window?.rootViewController
        while let presented = top?.presentedViewController, !presented.isBeingDismissed {
            top = presented
        }
        return top
    }
}

/// Sources that answer with configured images and failures, for the tests and
/// the DEBUG launch control `-leghost-image-fixture <scenario>`.
@MainActor
final class FixtureImageAttachmentSources: ImageAttachmentSources {
    enum Scenario: String {
        case png
        case jpeg
        case mismatch
        case unsupported
        case oversized
        case cancel
        case denied
        case revoked
        case unreadable
    }

    /// A 1x1 PNG.
    nonisolated static let png = Data(base64Encoded:
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")!

    let scenario: Scenario
    let limits: ImageAttachmentLimits
    private(set) var requests = 0

    init(scenario: Scenario, limits: ImageAttachmentLimits = .shared) {
        self.scenario = scenario
        self.limits = limits
    }

    func pickPhoto() async throws -> ImageAttachmentCandidate { try candidate(.photos) }
    func pickFile() async throws -> ImageAttachmentCandidate { try candidate(.files) }
    func readPasteboard() throws -> ImageAttachmentCandidate { try candidate(.pasteboard) }

    private func candidate(_ source: ImageAttachmentSource) throws -> ImageAttachmentCandidate {
        requests += 1
        let name = "fixture-\(source.rawValue)"
        switch scenario {
        case .png:
            return ImageAttachmentCandidate(data: Self.png, filename: name, declaredMediaType: "image/png")
        case .jpeg:
            let jpeg = Data([0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10] + Array("JFIF".utf8) + [0x00, 0xFF, 0xD9])
            return ImageAttachmentCandidate(data: jpeg, filename: name, declaredMediaType: "image/jpeg")
        case .mismatch:
            return ImageAttachmentCandidate(data: Self.png, filename: name, declaredMediaType: "image/jpeg")
        case .unsupported:
            return ImageAttachmentCandidate(data: Data("BM fixture bitmap".utf8), filename: name,
                                            declaredMediaType: nil)
        case .oversized:
            var data = Data(count: limits.maxBytesPerImage + 1)
            data.replaceSubrange(0..<Self.png.count, with: Self.png)
            return ImageAttachmentCandidate(data: data, filename: name, declaredMediaType: "image/png")
        case .cancel:
            throw ImageAttachmentFailure.cancelled
        case .denied:
            throw ImageAttachmentFailure.pasteDenied
        case .revoked:
            throw ImageAttachmentFailure.accessRevoked
        case .unreadable:
            throw ImageAttachmentFailure.unreadable
        }
    }
}
