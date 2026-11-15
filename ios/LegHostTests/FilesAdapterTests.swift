import XCTest
@testable import LegHost

/// A scratch tree for one adapter test: a Files provider, a folder to
/// export to, the guest home with its managed root, and the operation's
/// staging directory.
final class BridgeAdapterScratch {
    let root: URL
    let provider: URL
    let outside: URL
    let picked: URL
    let guestHome: URL
    let managedRoot: URL
    let staging: URL

    init() throws {
        root = FileManager.default.temporaryDirectory
            .appendingPathComponent("bridge-adapter-\(UUID().uuidString)", isDirectory: true)
        provider = root.appendingPathComponent("Provider", isDirectory: true)
        outside = root.appendingPathComponent("Outside", isDirectory: true)
        picked = root.appendingPathComponent("Picked", isDirectory: true)
        guestHome = root.appendingPathComponent("data/root", isDirectory: true)
        managedRoot = guestHome.appendingPathComponent(".local/share/leg/bridge", isDirectory: true)
        staging = root.appendingPathComponent("Caches/LegHost/BridgeStaging/g1/op1", isDirectory: true)
        for directory in [provider, outside, picked, guestHome] {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        }
    }

    deinit {
        try? FileManager.default.removeItem(at: root)
    }

    /// The managed copies the operations gave the guest, by copy id.
    private(set) var published: [String] = []
    /// Whether giving the guest a managed copy fails.
    var publishFails = false

    var context: BridgeOperationContext {
        BridgeOperationContext(generation: "g1", operationID: "op1", deadline: Date().addingTimeInterval(30),
                               paths: BridgeOperationPaths(staging: staging, managedRoot: managedRoot),
                               publishManagedCopy: { [unowned self] copyID in
                                   if publishFails {
                                       throw BridgeFailure.error("io_failure", "the guest could not be given the copy")
                                   }
                                   published.append(copyID)
                               })
    }

    @discardableResult
    func write(_ text: String, to url: URL) throws -> URL {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data(text.utf8).write(to: url)
        return url
    }

    func contents(of directory: URL) -> [String] {
        ((try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []).sorted()
    }

    func exists(_ url: URL) -> Bool {
        FileManager.default.fileExists(atPath: url.path)
    }

    /// Whether the managed root holds no copy.
    var managedRootIsEmpty: Bool { contents(of: managedRoot).isEmpty }
}

/// Runs an adapter call and returns what it answered.
func bridgeOutcome(_ body: () async throws -> BridgeJSON) async -> Result<BridgeJSON, BridgeFailure> {
    do {
        return .success(try await body())
    } catch let failure as BridgeFailure {
        return .failure(failure)
    } catch {
        return .failure(.error("unexpected_error", "\(error)"))
    }
}

extension Result where Failure == BridgeFailure {
    var value: Success? {
        if case .success(let value) = self { return value }
        return nil
    }

    var failure: BridgeFailure? {
        if case .failure(let failure) = self { return failure }
        return nil
    }
}

/// The Files adapter: picker-only import into the managed layout, export of
/// a managed copy, descriptor-relative containment, and the no-replace
/// commit.
final class FilesAdapterTests: XCTestCase {
    private var scratch: BridgeAdapterScratch!
    private var presenter: FixtureDocumentPresenter!
    private var adapter: FilesAdapter!

    override func setUpWithError() throws {
        scratch = try BridgeAdapterScratch()
        presenter = FixtureDocumentPresenter(importScopeRoot: scratch.provider)
        adapter = FilesAdapter(presenter: presenter)
    }

    override func tearDown() {
        adapter = nil
        presenter = nil
        scratch = nil
    }

    private func importFile() async -> Result<BridgeJSON, BridgeFailure> {
        await bridgeOutcome { try await adapter.perform(.filesImport, context: scratch.context) }
    }

    private func export(_ relativePath: String, filename: String? = nil) async -> Result<BridgeJSON, BridgeFailure> {
        let parts = relativePath.split(separator: "/", maxSplits: 1).map(String.init)
        let input = FilesExportInput(copyID: parts[0], name: parts.count > 1 ? parts[1] : "", filename: filename)
        return await bridgeOutcome { try await adapter.perform(.filesExport(input), context: scratch.context) }
    }

    /// Asserts a refusal that read nothing, staged nothing, and wrote nothing.
    private func assertRefused(_ result: Result<BridgeJSON, BridgeFailure>, _ status: BridgeReplyStatus,
                               _ reason: String, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertNil(result.value, file: file, line: line)
        XCTAssertEqual(result.failure?.status, status, file: file, line: line)
        XCTAssertEqual(result.failure?.reason, reason, file: file, line: line)
        XCTAssertEqual(adapter.sourceReads, 0, "the source was read", file: file, line: line)
        XCTAssertEqual(adapter.stagedOperations, 0, "content was staged", file: file, line: line)
        XCTAssertFalse(scratch.exists(scratch.staging), file: file, line: line)
        XCTAssertTrue(scratch.managedRootIsEmpty, file: file, line: line)
    }

    // MARK: - Import

    func testAnImportCopiesThePickedFileIntoTheManagedLayout() async throws {
        presenter.importURL = try scratch.write("report body", to: scratch.provider.appendingPathComponent("Docs/report.pdf"))

        let result = await importFile()

        let value = try XCTUnwrap(result.value, "\(String(describing: result.failure))")
        XCTAssertEqual(value.keys, ["copy_id", "relative_path", "name", "content_type", "size_bytes", "sha256"])
        let copyID = try XCTUnwrap(value["copy_id"]?.stringValue)
        XCTAssertTrue(BridgeValidation.isGeneratedID(copyID))
        XCTAssertEqual(value["relative_path"]?.stringValue, "\(copyID)/report.pdf")
        XCTAssertEqual(value["name"]?.stringValue, "report.pdf")
        XCTAssertEqual(value["content_type"]?.stringValue, "application/pdf")
        XCTAssertEqual(value["size_bytes"]?.integerValue, 11)
        XCTAssertEqual(value["sha256"]?.stringValue, BridgeCanonicalJSON.sha256Hex(Data("report body".utf8)))
        let copy = scratch.managedRoot.appendingPathComponent("\(copyID)/report.pdf")
        XCTAssertEqual(try String(contentsOf: copy), "report body")
        XCTAssertFalse(scratch.exists(scratch.staging), "the staging directory was left behind")
        XCTAssertEqual(presenter.openScopes, 0, "the security scope was left open")
        XCTAssertEqual(scratch.published, [copyID], "the guest was not given the copy")
    }

    func testACopyTheGuestCannotBeGivenIsRemoved() async throws {
        presenter.importURL = try scratch.write("body", to: scratch.provider.appendingPathComponent("a.txt"))
        scratch.publishFails = true

        let result = await importFile()

        XCTAssertEqual(result.failure?.status, .error)
        XCTAssertEqual(result.failure?.reason, "io_failure")
        XCTAssertTrue(scratch.managedRootIsEmpty, "a copy the guest cannot see was left behind")
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnImportThenAnExportRoundTripsTheRelativePath() async throws {
        presenter.importURL = try scratch.write("round trip", to: scratch.provider.appendingPathComponent("notes.txt"))
        presenter.exportDirectory = scratch.picked
        let importResult = await importFile()
        let imported = try XCTUnwrap(importResult.value)
        let relativePath = try XCTUnwrap(imported["relative_path"]?.stringValue)

        let result = await export(relativePath)

        let value = try XCTUnwrap(result.value, "\(String(describing: result.failure))")
        XCTAssertEqual(value.keys, ["export_id", "name", "content_type", "size_bytes", "sha256"])
        XCTAssertTrue(BridgeValidation.isGeneratedID(try XCTUnwrap(value["export_id"]?.stringValue)))
        XCTAssertEqual(value["name"]?.stringValue, "notes.txt")
        XCTAssertEqual(value["content_type"]?.stringValue, "text/plain")
        XCTAssertEqual(value["sha256"]?.stringValue, imported["sha256"]?.stringValue)
        XCTAssertEqual(try String(contentsOf: scratch.picked.appendingPathComponent("notes.txt")), "round trip")
        XCTAssertEqual(scratch.contents(of: scratch.picked), ["notes.txt"])
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnImportRefusesASymlinkedParentBeforeAnyRead() async throws {
        try scratch.write("secret", to: scratch.outside.appendingPathComponent("secret.txt"))
        try FileManager.default.createSymbolicLink(at: scratch.provider.appendingPathComponent("Link"),
                                                   withDestinationURL: scratch.outside)
        presenter.importURL = scratch.provider.appendingPathComponent("Link/secret.txt")

        assertRefused(await importFile(), .denied, "invalid_selection")
    }

    func testAnImportRefusesAFileOutsideTheScope() async throws {
        presenter.importURL = try scratch.write("secret", to: scratch.outside.appendingPathComponent("secret.txt"))

        assertRefused(await importFile(), .denied, "invalid_selection")
    }

    func testAnImportRefusesAParentComponentThatEscapesTheScope() async throws {
        try scratch.write("secret", to: scratch.outside.appendingPathComponent("secret.txt"))
        try FileManager.default.createDirectory(at: scratch.provider.appendingPathComponent("Docs"),
                                                withIntermediateDirectories: true)
        presenter.importURL = URL(fileURLWithPath: scratch.provider.path + "/Docs/../../Outside/secret.txt")

        assertRefused(await importFile(), .denied, "invalid_selection")
    }

    func testAnImportRefusesALinkAndADirectory() async throws {
        let target = try scratch.write("body", to: scratch.provider.appendingPathComponent("Docs/real.txt"))
        let link = scratch.provider.appendingPathComponent("link.txt")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: target)
        for url in [link, scratch.provider.appendingPathComponent("Docs")] {
            presenter.importURL = url
            assertRefused(await importFile(), .denied, "invalid_selection")
        }
    }

    func testAnImportRefusesAProviderWithoutAScopeRoot() async throws {
        presenter.importURL = try scratch.write("body", to: scratch.provider.appendingPathComponent("a.txt"))
        presenter.importScopeRoot = nil

        assertRefused(await importFile(), .denied, "invalid_selection")
    }

    func testAnImportRefusesAnUnsafeName() async throws {
        presenter.importURL = try scratch.write("body", to: scratch.provider.appendingPathComponent(" leading.txt"))

        assertRefused(await importFile(), .denied, "invalid_selection")
    }

    func testAnImportRefusesAnOversizedSourceBeforeAnyRead() async throws {
        presenter.importURL = try scratch.write("0123456789", to: scratch.provider.appendingPathComponent("big.bin"))
        adapter.transferLimit = 4

        assertRefused(await importFile(), .error, "size_limit")
    }

    func testAClosedPickerIsACancellation() async throws {
        presenter.importURL = nil

        assertRefused(await importFile(), .cancelled, "cancelled")
    }

    func testARevokedScopeIsUnavailable() async throws {
        presenter.importURL = try scratch.write("body", to: scratch.provider.appendingPathComponent("a.txt"))
        presenter.grantsAccess = false

        assertRefused(await importFile(), .unavailable, "permission_revoked")
    }

    func testAManagedDestinationThatAppearsBeforeTheCommitIsNotReplaced() async throws {
        presenter.importURL = try scratch.write("new body", to: scratch.provider.appendingPathComponent("a.txt"))
        var raced: URL?
        adapter.beforeCommit = { destination in
            raced = destination
            try? Data("old body".utf8).write(to: destination)
        }

        let result = await importFile()

        XCTAssertEqual(result.failure?.status, .denied)
        XCTAssertEqual(result.failure?.reason, "duplicate_destination")
        let destination = try XCTUnwrap(raced)
        XCTAssertEqual(try String(contentsOf: destination), "old body")
        XCTAssertEqual(scratch.contents(of: destination.deletingLastPathComponent()), ["a.txt"])
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    // MARK: - Export

    func testAnExportPreviewOpensNoPickerAndReadsNothing() async throws {
        let input = FilesExportInput(copyID: "abc", name: "a.txt", filename: nil)

        let review = try await adapter.preview(.filesExport(input), context: scratch.context)

        XCTAssertEqual(review["action"]?.stringValue, "files.export")
        XCTAssertEqual(review["relative_path"]?.stringValue, "abc/a.txt")
        XCTAssertEqual(presenter.picks, 0)
        XCTAssertEqual(adapter.sourceReads, 0)
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnExportOfAMissingCopyAsksNothing() async throws {
        presenter.exportDirectory = scratch.picked

        assertRefused(await export("abc/a.txt"), .denied, "invalid_selection")
        XCTAssertEqual(presenter.picks, 0)
    }

    func testAnExportRefusesASymlinkedCopyDirectory() async throws {
        try scratch.write("secret", to: scratch.outside.appendingPathComponent("a.txt"))
        try FileManager.default.createDirectory(at: scratch.managedRoot, withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(at: scratch.managedRoot.appendingPathComponent("abc"),
                                                   withDestinationURL: scratch.outside)
        presenter.exportDirectory = scratch.picked

        let result = await export("abc/a.txt")

        XCTAssertEqual(result.failure?.reason, "invalid_selection")
        XCTAssertEqual(adapter.sourceReads, 0)
        XCTAssertEqual(presenter.picks, 0)
        XCTAssertEqual(scratch.contents(of: scratch.picked), [])
    }

    func testAnExportRefusesASymlinkedManagedRoot() async throws {
        try scratch.write("secret", to: scratch.outside.appendingPathComponent("leg/bridge/abc/a.txt"))
        try FileManager.default.createDirectory(at: scratch.guestHome.appendingPathComponent(".local"),
                                                withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(at: scratch.guestHome.appendingPathComponent(".local/share"),
                                                   withDestinationURL: scratch.outside)
        presenter.exportDirectory = scratch.picked

        let result = await export("abc/a.txt")

        XCTAssertEqual(result.failure?.reason, "invalid_selection")
        XCTAssertEqual(adapter.sourceReads, 0)
        XCTAssertEqual(scratch.contents(of: scratch.picked), [])
    }

    func testAnExportUsesTheRequestedFilename() async throws {
        try scratch.write("body", to: scratch.managedRoot.appendingPathComponent("abc/a.txt"))
        presenter.exportDirectory = scratch.picked

        let result = await export("abc/a.txt", filename: "renamed.md")

        XCTAssertEqual(result.value?["name"]?.stringValue, "renamed.md")
        XCTAssertEqual(scratch.contents(of: scratch.picked), ["renamed.md"])
    }

    func testAnExportNeverReplacesAnExistingFile() async throws {
        try scratch.write("body", to: scratch.managedRoot.appendingPathComponent("abc/a.txt"))
        try scratch.write("old", to: scratch.picked.appendingPathComponent("a.txt"))
        presenter.exportDirectory = scratch.picked

        let result = await export("abc/a.txt")

        XCTAssertEqual(result.failure?.status, .denied)
        XCTAssertEqual(result.failure?.reason, "duplicate_destination")
        XCTAssertEqual(try String(contentsOf: scratch.picked.appendingPathComponent("a.txt")), "old")
        XCTAssertEqual(scratch.contents(of: scratch.picked), ["a.txt"])
    }

    func testAnExportDestinationThatAppearsBeforeTheCommitIsNotReplaced() async throws {
        try scratch.write("new", to: scratch.managedRoot.appendingPathComponent("abc/a.txt"))
        presenter.exportDirectory = scratch.picked
        adapter.beforeCommit = { destination in
            try? Data("old".utf8).write(to: destination)
        }

        let result = await export("abc/a.txt")

        XCTAssertEqual(result.failure?.reason, "duplicate_destination")
        XCTAssertEqual(try String(contentsOf: scratch.picked.appendingPathComponent("a.txt")), "old")
        XCTAssertEqual(scratch.contents(of: scratch.picked), ["a.txt"], "a partial copy was left behind")
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnExportToAClosedPickerIsACancellation() async throws {
        try scratch.write("body", to: scratch.managedRoot.appendingPathComponent("abc/a.txt"))
        presenter.exportDirectory = nil

        let result = await export("abc/a.txt")

        XCTAssertEqual(result.failure?.status, .cancelled)
        XCTAssertEqual(adapter.sourceReads, 0)
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    // MARK: - The transfer

    func testATransferRefusesAChunkThatPassesTheBoundBeforeWritingIt() throws {
        let transfer = try BridgeTransfer(staging: scratch.staging, limit: 8)
        try transfer.append(Data("12345".utf8))

        XCTAssertThrowsError(try transfer.append(Data("6789".utf8))) { error in
            XCTAssertEqual((error as? BridgeFailure)?.reason, "size_limit")
        }
        let written = try Data(contentsOf: transfer.payload)
        XCTAssertEqual(written, Data("12345".utf8), "the chunk past the bound was written")
        transfer.discard()
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testATransferVerifiesTheStagedFile() throws {
        let transfer = try BridgeTransfer(staging: scratch.staging)
        try transfer.append(Data("abc".utf8))

        let staged = try transfer.finish()

        XCTAssertEqual(staged.size, 3)
        XCTAssertEqual(staged.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        transfer.discard()
    }

    func testAContentTypeIsALowercaseMimeTypeOrOctetStream() {
        XCTAssertEqual(BridgeContentType.of("clip.mov"), "video/quicktime")
        XCTAssertEqual(BridgeContentType.of("README"), "application/octet-stream")
        XCTAssertEqual(BridgeContentType.of("data.unknownextension"), "application/octet-stream")
    }

    func testRelativeComponentsStayBelowTheRoot() {
        let root = URL(fileURLWithPath: "/private/var/mobile/Root")
        XCTAssertEqual(BridgeFileSystem.relativeComponents(of: URL(fileURLWithPath: "/var/mobile/Root/a/b.txt"), below: root),
                       ["a", "b.txt"])
        XCTAssertNil(BridgeFileSystem.relativeComponents(of: URL(fileURLWithPath: "/var/mobile/RootX/b.txt"), below: root))
        XCTAssertNil(BridgeFileSystem.relativeComponents(of: URL(fileURLWithPath: "/var/mobile/Root/../b.txt"), below: root))
        XCTAssertNil(BridgeFileSystem.relativeComponents(of: root, below: root))
    }
}
