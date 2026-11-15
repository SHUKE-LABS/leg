import Foundation
import UIKit
import UniformTypeIdentifiers

// MARK: - The document picker

/// The Files picker the adapters ask the user through, and the security
/// scope of what the user picked.
///
/// Picking is the consent: the host never reads a file or writes a folder the
/// user did not pick, and it never takes a native path from the guest.
protocol BridgeDocumentPresenter: AnyObject {
    /// Asks the user for one file to copy into the guest workspace. A user
    /// who closes the picker ends the request with `BridgeFailure.cancelled`.
    func pickFileToImport() async throws -> URL

    /// Asks the user for the folder a copy is saved to.
    func pickExportDirectory() async throws -> URL

    /// Opens the security scope of a picked URL; false when the provider no
    /// longer grants it.
    func startAccessing(_ url: URL) -> Bool

    /// Closes the security scope `startAccessing` opened.
    func stopAccessing(_ url: URL)

    /// The directory that proves a picked file is inside its provider's
    /// scope, or nil when the provider exposes none.
    func scopeRoot(for url: URL) -> URL?
}

extension BridgeDocumentPresenter {
    func startAccessing(_ url: URL) -> Bool {
        url.startAccessingSecurityScopedResource()
    }

    func stopAccessing(_ url: URL) {
        url.stopAccessingSecurityScopedResource()
    }

    /// The volume the picked file lives on.
    ///
    /// The volume is the one root a provider always exposes; every component
    /// below it is walked without following a link, so a link anywhere on
    /// the way to the file is refused.
    func scopeRoot(for url: URL) -> URL? {
        (try? url.resourceValues(forKeys: [.volumeURLKey]))?.volume
    }
}

/// The picker of the running app: `UIDocumentPickerViewController`, shown
/// over the top-most view controller of the key window.
///
/// The picker opens files in place, so the URL it returns is security
/// scoped. A request whose task is cancelled — at the deadline or at
/// teardown — dismisses the picker it showed.
@MainActor
final class LiveDocumentPresenter: BridgeDocumentPresenter {
    nonisolated init() {}

    func pickFileToImport() async throws -> URL {
        try await present(UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: false))
    }

    func pickExportDirectory() async throws -> URL {
        try await present(UIDocumentPickerViewController(forOpeningContentTypes: [.folder]))
    }

    private func present(_ picker: UIDocumentPickerViewController) async throws -> URL {
        picker.allowsMultipleSelection = false
        let session = PickerSession(picker: picker)
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                session.start(continuation)
            }
        } onCancel: {
            Task { @MainActor in session.cancel() }
        }
    }

    /// One picker on screen and the request waiting for it.
    @MainActor
    private final class PickerSession: NSObject, UIDocumentPickerDelegate {
        private let picker: UIDocumentPickerViewController
        private var continuation: CheckedContinuation<URL, Error>?
        private var cancelled = false

        init(picker: UIDocumentPickerViewController) {
            self.picker = picker
        }

        func start(_ continuation: CheckedContinuation<URL, Error>) {
            guard !cancelled else {
                continuation.resume(throwing: BridgeFailure.cancelled)
                return
            }
            guard let presenter = Self.topViewController() else {
                continuation.resume(throwing: BridgeFailure.unavailable(
                    "picker_unavailable", "the app has no window to show the Files picker in"))
                return
            }
            self.continuation = continuation
            picker.delegate = self
            presenter.present(picker, animated: true)
        }

        func cancel() {
            cancelled = true
            guard continuation != nil else { return }
            picker.dismiss(animated: true)
            finish(.failure(BridgeFailure.cancelled))
        }

        func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
            if let url = urls.first {
                finish(.success(url))
            } else {
                finish(.failure(BridgeFailure.cancelled))
            }
        }

        func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
            finish(.failure(BridgeFailure.cancelled))
        }

        private func finish(_ result: Result<URL, Error>) {
            guard let continuation else { return }
            self.continuation = nil
            continuation.resume(with: result)
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
}

/// A picker that answers with configured URLs, for the tests and the DEBUG
/// fixture of the app. A URL that is nil is a user who closed the picker.
final class FixtureDocumentPresenter: BridgeDocumentPresenter {
    var importURL: URL?
    var exportDirectory: URL?
    /// The scope root of a picked file; nil is a provider that exposes none.
    var importScopeRoot: URL?
    /// Whether the provider still grants the scope of what was picked.
    var grantsAccess = true
    private(set) var picks = 0
    private(set) var openScopes = 0

    init(importURL: URL? = nil, exportDirectory: URL? = nil, importScopeRoot: URL? = nil) {
        self.importURL = importURL
        self.exportDirectory = exportDirectory
        self.importScopeRoot = importScopeRoot
    }

    func pickFileToImport() async throws -> URL {
        picks += 1
        guard let importURL else { throw BridgeFailure.cancelled }
        return importURL
    }

    func pickExportDirectory() async throws -> URL {
        picks += 1
        guard let exportDirectory else { throw BridgeFailure.cancelled }
        return exportDirectory
    }

    func startAccessing(_ url: URL) -> Bool {
        if grantsAccess { openScopes += 1 }
        return grantsAccess
    }

    func stopAccessing(_ url: URL) {
        openScopes -= 1
    }

    func scopeRoot(for url: URL) -> URL? {
        importScopeRoot
    }
}

// MARK: - The Files adapter

/// Serves `files.import` and `files.export`.
///
/// An import copies one picked regular file into the managed layout of the
/// guest home, `~/.local/share/leg/bridge/<copy_id>/<name>`; an export saves
/// a copy of one managed file into a folder the user picks. Nothing is read
/// before every component of the source was proven to be inside its scope,
/// and nothing that exists is ever overwritten.
final class FilesAdapter: BridgeCapabilityAdapter {
    private let presenter: BridgeDocumentPresenter
    /// The transfer bound; the tests lower it.
    var transferLimit = BridgeLimits.maxTransferBytes
    /// Runs between the preflight of a destination and its commit; the tests
    /// put a file there to race the commit.
    var beforeCommit: ((URL) -> Void)?
    /// How many sources were read, so a test proves a refusal read nothing.
    private(set) var sourceReads = 0
    /// How many operations staged content.
    private(set) var stagedOperations = 0

    init(presenter: BridgeDocumentPresenter) {
        self.presenter = presenter
    }

    func preview(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON {
        guard case .filesExport(let input) = request else {
            throw BridgeFailure.protocolViolation("\(request.capability) runs without an approval")
        }
        let name = input.filename ?? input.name
        return .object([
            "action": .string("files.export"),
            "summary": .string("Save a copy of the guest file \(input.copyID)/\(input.name) as \(name) in a folder you pick in Files"),
            "relative_path": .string("\(input.copyID)/\(input.name)"),
            "name": .string(name),
            "destination": .string("files_picker"),
        ])
    }

    func perform(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON {
        switch request {
        case .filesImport:
            return try await importFile(context: context)
        case .filesExport(let input):
            return try await exportFile(input, context: context)
        default:
            throw BridgeFailure.unavailable("capability_unsupported", "the Files adapter does not serve \(request.capability)")
        }
    }

    private func importFile(context: BridgeOperationContext) async throws -> BridgeJSON {
        let url = try await presenter.pickFileToImport()
        guard presenter.startAccessing(url) else {
            throw BridgeFailure.unavailable("permission_revoked", "the Files provider no longer grants the picked file")
        }
        defer { presenter.stopAccessing(url) }
        let name = url.lastPathComponent
        guard BridgeValidation.isSafeBasename(name) else {
            throw BridgeFailure.denied("invalid_selection", "the picked file has a name the host does not copy")
        }
        guard let root = presenter.scopeRoot(for: url) else {
            throw BridgeFailure.denied("invalid_selection", "the Files provider exposes no root that proves the file is in its scope")
        }
        let copyID = BridgeValidation.newGeneratedID()
        let limit = transferLimit
        let staged: BridgeStagedFile = try await BridgeIO.run(deadline: context.deadline) { [self] flag in
            try BridgeFileSystem.coordinatedRead(url) { readURL in
                let source = try BridgeFileSystem.openContained(readURL, root: root)
                defer { close(source.descriptor) }
                guard source.size <= limit else {
                    throw BridgeFailure.error("size_limit", "the picked file is larger than the \(limit) byte limit")
                }
                return try stage(from: source.descriptor, context: context, limit: limit, flag: flag) { transfer in
                    try BridgeCommit.commitToManagedRoot(transfer, managedRoot: context.paths.managedRoot,
                                                         copyID: copyID, name: name,
                                                         publish: context.publishManagedCopy,
                                                         beforeCommit: beforeCommit)
                }
            }
        }
        return .object([
            "copy_id": .string(copyID),
            "relative_path": .string("\(copyID)/\(name)"),
            "name": .string(name),
            "content_type": .string(BridgeContentType.of(name)),
            "size_bytes": .integer(staged.size),
            "sha256": .string(staged.sha256),
        ])
    }

    private func exportFile(_ input: FilesExportInput, context: BridgeOperationContext) async throws -> BridgeJSON {
        let managedRoot = context.paths.managedRoot
        let limit = transferLimit
        // The source is proven before the picker opens, so a request for a
        // file that is not a managed copy never asks the user anything.
        let size = try BridgeFileSystem.withManagedCopy(managedRoot: managedRoot, copyID: input.copyID,
                                                        name: input.name) { $0.size }
        guard size <= limit else {
            throw BridgeFailure.error("size_limit", "the guest file is larger than the \(limit) byte limit")
        }
        let directory = try await presenter.pickExportDirectory()
        guard presenter.startAccessing(directory) else {
            throw BridgeFailure.unavailable("permission_revoked", "the Files provider no longer grants the picked folder")
        }
        defer { presenter.stopAccessing(directory) }
        let name = input.filename ?? input.name
        let staged: BridgeStagedFile = try await BridgeIO.run(deadline: context.deadline) { [self] flag in
            try BridgeFileSystem.withManagedCopy(managedRoot: managedRoot, copyID: input.copyID, name: input.name) { source in
                guard source.size <= limit else {
                    throw BridgeFailure.error("size_limit", "the guest file is larger than the \(limit) byte limit")
                }
                return try stage(from: source.descriptor, context: context, limit: limit, flag: flag) { transfer in
                    try BridgeCommit.commitToDirectory(transfer, directory: directory, name: name,
                                                       operationID: context.operationID, flag: flag,
                                                       beforeCommit: beforeCommit)
                }
            }
        }
        return .object([
            "export_id": .string(BridgeValidation.newGeneratedID()),
            "name": .string(name),
            "content_type": .string(BridgeContentType.of(name)),
            "size_bytes": .integer(staged.size),
            "sha256": .string(staged.sha256),
        ])
    }

    /// Streams `descriptor` into the operation's staging directory, verifies
    /// the staged file, and hands it to `commit`. The staging directory is
    /// gone when this returns, whatever happened.
    private func stage(from descriptor: Int32, context: BridgeOperationContext, limit: Int64,
                       flag: BridgeCancelFlag, commit: (BridgeStagedFile) throws -> Void) throws -> BridgeStagedFile {
        stagedOperations += 1
        let transfer = try BridgeTransfer(staging: context.paths.staging, limit: limit)
        defer { transfer.discard() }
        var buffer = [UInt8](repeating: 0, count: BridgeTransfer.chunkBytes)
        var first = true
        while true {
            try flag.check()
            let count = buffer.withUnsafeMutableBytes { read(descriptor, $0.baseAddress, $0.count) }
            if first {
                sourceReads += 1
                first = false
            }
            if count < 0 {
                if errno == EINTR { continue }
                throw BridgeFailure.posix(errno, "the source could not be read")
            }
            if count == 0 { break }
            try transfer.append(buffer[0..<count])
        }
        let staged = try transfer.finish()
        try flag.check()
        try commit(staged)
        return staged
    }
}

// MARK: - The transfer pipeline

/// A file staged for commit, verified by size and digest.
struct BridgeStagedFile {
    /// The staged content; it is renamed or copied, never edited.
    let url: URL
    let size: Int64
    let sha256: String
}

/// One transfer into the operation's staging directory.
///
/// The byte count is checked before a chunk is written, so a source that is
/// larger than the bound never leaves more than the bound on disk; the
/// SHA-256 is computed as the bytes pass. `finish` flushes the file to the
/// disk and reads it back, so what is committed is what was counted.
final class BridgeTransfer {
    static let chunkBytes = 256 * 1024
    static let payloadName = "payload"

    let directory: URL
    let limit: Int64
    private var descriptor: Int32
    private var context = sha256_ctx()
    private(set) var size: Int64 = 0

    /// Creates the staging directory and an empty payload in it.
    init(staging: URL, limit: Int64 = BridgeLimits.maxTransferBytes) throws {
        directory = staging
        self.limit = limit
        do {
            try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        } catch {
            throw BridgeFailure.from(error)
        }
        descriptor = open(staging.appendingPathComponent(Self.payloadName).path,
                          O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard descriptor >= 0 else {
            let code = errno
            try? FileManager.default.removeItem(at: staging)
            throw BridgeFailure.posix(code, "the staging file could not be created")
        }
        sha256_init(&context)
    }

    deinit {
        if descriptor >= 0 { close(descriptor) }
    }

    var payload: URL { directory.appendingPathComponent(Self.payloadName) }

    /// Adds one chunk; a chunk that would pass the bound is refused before
    /// any of it is written.
    func append<Bytes: Collection>(_ bytes: Bytes) throws where Bytes.Element == UInt8 {
        guard descriptor >= 0 else { throw BridgeFailure.error("io_failure", "the transfer is closed") }
        guard size + Int64(bytes.count) <= limit else {
            throw BridgeFailure.error("size_limit", "the content is larger than the \(limit) byte limit")
        }
        let chunk = Array(bytes)
        try chunk.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            sha256_update(&context, base, raw.count)
            try BridgeFileSystem.writeAll(descriptor, base, raw.count)
        }
        size += Int64(chunk.count)
    }

    /// Flushes the payload, then reads it back and checks its size and digest.
    func finish() throws -> BridgeStagedFile {
        var digest = [UInt8](repeating: 0, count: 32)
        sha256_final(&context, &digest)
        let expected = BridgeFileSystem.hex(digest)
        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: false)
        do {
            try handle.synchronize()
        } catch {
            throw BridgeFailure.from(error)
        }
        close(descriptor)
        descriptor = -1
        let check = open(payload.path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard check >= 0 else { throw BridgeFailure.posix(errno, "the staged file could not be read back") }
        defer { close(check) }
        let (size, sha256) = try BridgeFileSystem.digest(of: check, flag: nil)
        guard size == self.size, sha256 == expected else {
            throw BridgeFailure.error("io_failure", "the staged file does not match what was transferred")
        }
        return BridgeStagedFile(url: payload, size: size, sha256: sha256)
    }

    /// Removes the operation's staging directory, and only it.
    func discard() {
        if descriptor >= 0 {
            close(descriptor)
            descriptor = -1
        }
        try? FileManager.default.removeItem(at: directory)
    }
}

/// The atomic, no-replace commits of a staged file.
enum BridgeCommit {
    /// The guest-home components of the managed root, walked without
    /// following a link and created when missing.
    static let managedComponents = [".local", "share", "leg", "bridge"]

    /// Moves a staged file to `<managed root>/<copy_id>/<name>`.
    ///
    /// The rename is `renameatx_np(RENAME_EXCL)`, so a destination that
    /// appeared after the preflight makes the commit fail with
    /// `duplicate_destination` and is left as it was.
    static func commitToManagedRoot(_ staged: BridgeStagedFile, managedRoot: URL, copyID: String, name: String,
                                    publish: (String) throws -> Void,
                                    beforeCommit: ((URL) -> Void)?) throws {
        let root = try BridgeFileSystem.openManagedRoot(managedRoot, create: true)
        defer { close(root) }
        if mkdirat(root, copyID, 0o755) != 0 {
            let code = errno
            if code == EEXIST {
                throw BridgeFailure.denied("duplicate_destination", "the managed copy already exists")
            }
            throw BridgeFailure.posix(code, "the managed copy directory could not be created")
        }
        var committed = false
        defer {
            // Only the empty directory this commit created is removed; a
            // directory that holds anything is left alone.
            if !committed { unlinkat(root, copyID, AT_REMOVEDIR) }
        }
        let copy = openat(root, copyID, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard copy >= 0 else { throw BridgeFailure.posix(errno, "the managed copy directory could not be opened") }
        defer { close(copy) }
        try BridgeFileSystem.requireAbsent(copy, name)
        beforeCommit?(managedRoot.appendingPathComponent(copyID).appendingPathComponent(name))
        let staging = open(staged.url.deletingLastPathComponent().path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard staging >= 0 else { throw BridgeFailure.posix(errno, "the staging directory could not be opened") }
        defer { close(staging) }
        try BridgeFileSystem.renameExclusive(staging, staged.url.lastPathComponent, copy, name)
        fsync(copy)
        do {
            try publish(copyID)
        } catch {
            // A copy the guest cannot see is no copy: it goes with its
            // directory.
            unlinkat(copy, name, 0)
            throw error
        }
        committed = true
    }

    /// Copies a staged file into a picked folder as `name`.
    ///
    /// The copy goes to a temporary sibling the commit creates exclusively,
    /// and the sibling is renamed with `renameatx_np(RENAME_EXCL)` under a
    /// coordinated write of the folder. A provider that cannot rename
    /// exclusively fails the commit with `io_failure`; a destination that
    /// exists fails it with `duplicate_destination`. The sibling is removed
    /// whenever the commit does not finish.
    static func commitToDirectory(_ staged: BridgeStagedFile, directory: URL, name: String, operationID: String,
                                  flag: BridgeCancelFlag?, beforeCommit: ((URL) -> Void)?) throws {
        try BridgeFileSystem.coordinatedWrite(directory) { folder in
            let parent = open(folder.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
            guard parent >= 0 else { throw BridgeFailure.posix(errno, "the picked folder could not be opened") }
            defer { close(parent) }
            try BridgeFileSystem.requireAbsent(parent, name)
            let sibling = ".leg-export-\(operationID).partial"
            let target = openat(parent, sibling, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o644)
            guard target >= 0 else { throw BridgeFailure.posix(errno, "the temporary copy could not be created") }
            var renamed = false
            defer {
                if !renamed { unlinkat(parent, sibling, 0) }
            }
            do {
                defer { close(target) }
                let source = open(staged.url.path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
                guard source >= 0 else { throw BridgeFailure.posix(errno, "the staged file could not be read") }
                defer { close(source) }
                let (size, sha256) = try BridgeFileSystem.digest(of: source, flag: flag, copyingTo: target)
                guard size == staged.size, sha256 == staged.sha256 else {
                    throw BridgeFailure.error("io_failure", "the copy does not match the staged file")
                }
                do {
                    try FileHandle(fileDescriptor: target, closeOnDealloc: false).synchronize()
                } catch {
                    throw BridgeFailure.from(error)
                }
            }
            try flag?.check()
            beforeCommit?(folder.appendingPathComponent(name))
            try BridgeFileSystem.renameExclusive(parent, sibling, parent, name)
            renamed = true
            fsync(parent)
        }
    }
}

/// A file opened for reading after its path was proven.
struct BridgeOpenedSource {
    let descriptor: Int32
    let size: Int64
}

/// The descriptor-relative file system work of the adapters.
enum BridgeFileSystem {
    /// Opens the regular file at `url`, walking every component below
    /// `root` relative to a descriptor: a parent is checked with
    /// `fstatat(AT_SYMLINK_NOFOLLOW)` and opened with
    /// `O_DIRECTORY|O_NOFOLLOW`, and the file itself is checked, opened with
    /// `O_NOFOLLOW`, and checked again by its descriptor. A path outside
    /// `root`, a link anywhere on the way, and anything that is not a regular
    /// file are `invalid_selection`.
    static func openContained(_ url: URL, root: URL) throws -> BridgeOpenedSource {
        guard let components = relativeComponents(of: url, below: root), !components.isEmpty else {
            throw invalidSelection("the picked file is outside the scope of its provider")
        }
        let rootDescriptor = open(root.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard rootDescriptor >= 0 else {
            throw invalidSelection("the scope of the Files provider cannot be opened to prove the file is in it")
        }
        return try openRegular(below: rootDescriptor, components: components)
    }

    /// Runs `body` with the managed copy `<managed root>/<copy_id>/<name>`
    /// open, walked like a picked file. A copy that is missing, a link, or
    /// not a regular file is `invalid_selection`.
    static func withManagedCopy<T>(managedRoot: URL, copyID: String, name: String,
                                   _ body: (BridgeOpenedSource) throws -> T) throws -> T {
        let root: Int32
        do {
            root = try openManagedRoot(managedRoot, create: false)
        } catch {
            throw invalidSelection("the guest holds no managed copy \(copyID)/\(name)")
        }
        let source = try openRegular(below: root, components: [copyID, name])
        defer { close(source.descriptor) }
        return try body(source)
    }

    /// Opens the managed root: the guest home is trusted as the host made
    /// it, and `.local/share/leg/bridge` below it is walked without following
    /// a link and created when `create` is set.
    static func openManagedRoot(_ managedRoot: URL, create: Bool) throws -> Int32 {
        let all = managedRoot.standardizedFileURL.pathComponents
        var anchor = managedRoot
        var walk: [String] = []
        if all.count > BridgeCommit.managedComponents.count,
           Array(all.suffix(BridgeCommit.managedComponents.count)) == BridgeCommit.managedComponents {
            walk = BridgeCommit.managedComponents
            for _ in walk { anchor.deleteLastPathComponent() }
        }
        if create {
            do {
                try FileManager.default.createDirectory(at: anchor, withIntermediateDirectories: true)
            } catch {
                throw BridgeFailure.from(error)
            }
        }
        var current = open(anchor.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard current >= 0 else { throw posix(errno, "the guest home could not be opened") }
        for component in walk {
            if create, mkdirat(current, component, 0o755) != 0, errno != EEXIST {
                let code = errno
                close(current)
                throw posix(code, "the managed directory could not be created")
            }
            let next = try openDirectory(current, component)
            close(current)
            current = next
        }
        return current
    }

    /// Fails with `duplicate_destination` when `name` exists in `directory`.
    static func requireAbsent(_ directory: Int32, _ name: String) throws {
        var status = stat()
        if fstatat(directory, name, &status, AT_SYMLINK_NOFOLLOW) == 0 {
            throw BridgeFailure.denied("duplicate_destination", "the destination already holds \(name)")
        }
        if errno != ENOENT {
            throw posix(errno, "the destination could not be checked")
        }
    }

    /// Renames without replacing: an existing destination is
    /// `duplicate_destination`, a provider that cannot rename exclusively is
    /// `io_failure`.
    static func renameExclusive(_ fromDirectory: Int32, _ from: String, _ toDirectory: Int32, _ to: String) throws {
        if renameatx_np(fromDirectory, from, toDirectory, to, UInt32(RENAME_EXCL)) == 0 { return }
        let code = errno
        switch code {
        case EEXIST:
            throw BridgeFailure.denied("duplicate_destination", "the destination appeared before the commit")
        case ENOTSUP, EINVAL, EXDEV:
            throw BridgeFailure.error("io_failure", "the destination cannot be committed without the risk of replacing a file")
        default:
            throw posix(code, "the commit failed")
        }
    }

    /// Reads `descriptor` to its end and returns the byte count and the
    /// SHA-256, copying the bytes to `target` when one is given.
    static func digest(of descriptor: Int32, flag: BridgeCancelFlag?, copyingTo target: Int32? = nil) throws -> (Int64, String) {
        var context = sha256_ctx()
        sha256_init(&context)
        var buffer = [UInt8](repeating: 0, count: BridgeTransfer.chunkBytes)
        var size: Int64 = 0
        while true {
            try flag?.check()
            let count = buffer.withUnsafeMutableBytes { read(descriptor, $0.baseAddress, $0.count) }
            if count < 0 {
                if errno == EINTR { continue }
                throw posix(errno, "the file could not be read")
            }
            if count == 0 { break }
            try buffer.withUnsafeBytes { raw in
                sha256_update(&context, raw.baseAddress!, count)
                if let target { try writeAll(target, raw.baseAddress!, count) }
            }
            size += Int64(count)
        }
        var digest = [UInt8](repeating: 0, count: 32)
        sha256_final(&context, &digest)
        return (size, hex(digest))
    }

    static func writeAll(_ descriptor: Int32, _ base: UnsafeRawPointer, _ count: Int) throws {
        var written = 0
        while written < count {
            let result = write(descriptor, base + written, count - written)
            if result < 0 {
                if errno == EINTR { continue }
                throw posix(errno, "the file could not be written")
            }
            written += result
        }
    }

    /// Runs `body` under a coordinated read of `url`.
    static func coordinatedRead<T>(_ url: URL, _ body: (URL) throws -> T) throws -> T {
        var outcome: Result<T, Error>?
        var coordinationError: NSError?
        withoutActuallyEscaping(body) { body in
            NSFileCoordinator().coordinate(readingItemAt: url, options: [.withoutChanges],
                                           error: &coordinationError) { readURL in
                outcome = Result { try body(readURL) }
            }
        }
        return try coordinationOutcome(outcome, coordinationError)
    }

    /// Runs `body` under a coordinated write of `url`.
    static func coordinatedWrite<T>(_ url: URL, _ body: (URL) throws -> T) throws -> T {
        var outcome: Result<T, Error>?
        var coordinationError: NSError?
        withoutActuallyEscaping(body) { body in
            NSFileCoordinator().coordinate(writingItemAt: url, options: [],
                                           error: &coordinationError) { writeURL in
                outcome = Result { try body(writeURL) }
            }
        }
        return try coordinationOutcome(outcome, coordinationError)
    }

    private static func coordinationOutcome<T>(_ outcome: Result<T, Error>?, _ error: NSError?) throws -> T {
        if let outcome { return try outcome.get() }
        throw BridgeFailure.error("io_failure", "the Files provider refused the access")
    }

    static func hex(_ bytes: [UInt8]) -> String {
        bytes.map { String(format: "%02x", $0) }.joined()
    }

    static func posix(_ code: Int32, _ message: String) -> BridgeFailure {
        BridgeFailure.posix(code, message)
    }

    private static func invalidSelection(_ message: String) -> BridgeFailure {
        .denied("invalid_selection", message)
    }

    /// The components of `url` below `root`, compared lexically; nil when
    /// `url` is not below `root` or carries a `.` or `..` component.
    static func relativeComponents(of url: URL, below root: URL) -> [String]? {
        let path = aliasFree(url.path)
        let rootPath = aliasFree(root.path)
        let rootComponents = rootPath.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
        let components = path.split(separator: "/", omittingEmptySubsequences: false).dropFirst().map(String.init)
        guard path.hasPrefix("/"), components.count > rootComponents.count,
              Array(components.prefix(rootComponents.count)) == rootComponents else { return nil }
        let rest = Array(components.dropFirst(rootComponents.count))
        guard rest.allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else { return nil }
        return rest
    }

    /// The path with the `/private` prefix of the Darwin aliases taken off,
    /// so a URL a provider spelled through `/var` compares with its root.
    private static func aliasFree(_ path: String) -> String {
        for alias in ["/private/var/", "/private/tmp/"] where path.hasPrefix(alias) {
            return String(path.dropFirst("/private".count))
        }
        if path == "/private/var" || path == "/private/tmp" { return String(path.dropFirst("/private".count)) }
        return path
    }

    private static func openDirectory(_ parent: Int32, _ name: String) throws -> Int32 {
        var status = stat()
        guard fstatat(parent, name, &status, AT_SYMLINK_NOFOLLOW) == 0 else {
            throw invalidSelection("a directory on the way to the file is missing")
        }
        guard status.st_mode & S_IFMT == S_IFDIR else {
            throw invalidSelection("a component on the way to the file is a link or not a directory")
        }
        let descriptor = openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard descriptor >= 0 else {
            throw invalidSelection("a directory on the way to the file cannot be opened")
        }
        return descriptor
    }

    /// Opens the regular file at `components` below `root`, which it closes.
    private static func openRegular(below root: Int32, components: [String]) throws -> BridgeOpenedSource {
        var current = root
        defer { close(current) }
        for component in components.dropLast() {
            let next = try openDirectory(current, component)
            close(current)
            current = next
        }
        let name = components.last!
        var before = stat()
        guard fstatat(current, name, &before, AT_SYMLINK_NOFOLLOW) == 0 else {
            throw invalidSelection("the file is missing")
        }
        guard before.st_mode & S_IFMT == S_IFREG else {
            throw invalidSelection("the selection is a link or not a regular file")
        }
        let descriptor = openat(current, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard descriptor >= 0 else { throw invalidSelection("the file cannot be opened") }
        var after = stat()
        guard fstat(descriptor, &after) == 0, after.st_mode & S_IFMT == S_IFREG,
              after.st_dev == before.st_dev, after.st_ino == before.st_ino else {
            close(descriptor)
            throw invalidSelection("the file changed while it was opened")
        }
        return BridgeOpenedSource(descriptor: descriptor, size: Int64(after.st_size))
    }
}

/// The content type of a file name: the lowercase MIME type of its
/// extension, or `application/octet-stream`.
enum BridgeContentType {
    static func of(_ name: String) -> String {
        let fallback = "application/octet-stream"
        let pathExtension = (name as NSString).pathExtension
        guard !pathExtension.isEmpty, let type = UTType(filenameExtension: pathExtension),
              let mime = type.preferredMIMEType?.lowercased(),
              BridgeValidation.isContentType(mime) else { return fallback }
        return mime
    }
}

// MARK: - Cancellation and blocking work

/// The cancellation a blocking transfer polls between chunks: the task's
/// cancellation and the request deadline.
final class BridgeCancelFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var cancelled = false
    let deadline: Date?

    init(deadline: Date? = nil) {
        self.deadline = deadline
    }

    func cancel() {
        lock.lock()
        cancelled = true
        lock.unlock()
    }

    var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    func check() throws {
        if isCancelled { throw BridgeFailure.cancelled }
        if let deadline, Date() > deadline { throw BridgeFailure.timeout }
    }
}

enum BridgeIO {
    /// Runs blocking file work off the cooperative pool; the task's
    /// cancellation reaches the work through the flag it polls.
    static func run<T>(deadline: Date?, _ body: @escaping (BridgeCancelFlag) throws -> T) async throws -> T {
        let flag = BridgeCancelFlag(deadline: deadline)
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                DispatchQueue.global(qos: .userInitiated).async {
                    do {
                        continuation.resume(returning: try body(flag))
                    } catch {
                        continuation.resume(throwing: BridgeFailure.from(error))
                    }
                }
            }
        } onCancel: {
            flag.cancel()
        }
    }
}

extension BridgeFailure {
    /// The failure an errno is: no space is `insufficient_space`, anything
    /// else `io_failure`.
    static func posix(_ code: Int32, _ message: String) -> BridgeFailure {
        if code == ENOSPC || code == EDQUOT {
            return .error("insufficient_space", "\(message): the device has no space left")
        }
        return .error("io_failure", "\(message) (errno \(code))")
    }

    /// The failure any error is, for a transfer.
    static func from(_ error: Error) -> BridgeFailure {
        if let failure = error as? BridgeFailure { return failure }
        if error is CancellationError { return .cancelled }
        let nsError = error as NSError
        if nsError.domain == NSPOSIXErrorDomain {
            return .posix(Int32(nsError.code), "the transfer failed")
        }
        if nsError.domain == NSCocoaErrorDomain, nsError.code == NSFileWriteOutOfSpaceError {
            return .error("insufficient_space", "the device has no space left")
        }
        if nsError.domain == NSCocoaErrorDomain, nsError.code == NSUserCancelledError {
            return .cancelled
        }
        return .error("io_failure", "the transfer failed")
    }
}
