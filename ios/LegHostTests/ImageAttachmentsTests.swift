import UIKit
import UniformTypeIdentifiers
import XCTest
@testable import LegHost

/// The image attachment path of the host: validation, staging, the guest
/// handoff, and the sources.
final class ImageAttachmentsTests: XCTestCase {
    private var root: URL!
    private let png = FixtureImageAttachmentSources.png
    private let pngSHA256 = "497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581"

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory
            .appendingPathComponent("image-attachments-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    // MARK: Validation and staging

    func testMagicBytesProveTheType() {
        XCTAssertEqual(ImageMediaType.detect(png), "image/png")
        XCTAssertEqual(ImageMediaType.detect(Data([0xFF, 0xD8, 0xFF, 0xE0])), "image/jpeg")
        XCTAssertEqual(ImageMediaType.detect(Data("GIF87a....".utf8)), "image/gif")
        XCTAssertEqual(ImageMediaType.detect(Data("GIF89a....".utf8)), "image/gif")
        XCTAssertEqual(ImageMediaType.detect(Data("RIFF\0\0\0\0WEBPVP8 ".utf8)), "image/webp")
        XCTAssertNil(ImageMediaType.detect(Data("RIFF\0\0\0\0WAVEfmt ".utf8)))
        XCTAssertNil(ImageMediaType.detect(Data("BM bitmap".utf8)))
        XCTAssertNil(ImageMediaType.detect(Data()))
    }

    func testAStagedImageCarriesOnlySafeMetadata() throws {
        let store = makeStore()
        let staged = try store.stage(candidate(png, name: "../../private/var/mobile/IMG_0001", type: "image/png"),
                                     source: .photos, generation: "g1")
        XCTAssertTrue(staged.id.hasPrefix("ios-"))
        XCTAssertEqual(staged.id.count, 36)
        XCTAssertEqual(staged.filename, "IMG_0001.png")
        XCTAssertEqual(staged.label, "Photos · IMG_0001.png")
        XCTAssertEqual(staged.mediaType, "image/png")
        XCTAssertEqual(staged.byteCount, png.count)
        XCTAssertEqual(staged.sha256, pngSHA256)
        XCTAssertEqual(contents(of: generationDirectory(store)), [staged.id])
        XCTAssertEqual(try Data(contentsOf: store.fileURL(for: staged)), png)
        XCTAssertEqual(store.pending, [staged])
    }

    func testRejectedImagesLeaveNoFile() throws {
        let limits = ImageAttachmentLimits(maxEncodedBytesPerImage: 200, maxEncodedBytesPerTurn: 200)
        let store = makeStore(limits: limits)
        let cases: [(ImageAttachmentCandidate, ImageAttachmentFailure)] = [
            (candidate(png, type: "image/jpeg"), .typeMismatch),
            (candidate(Data("BM bitmap".utf8)), .unsupportedType),
            (candidate(Data()), .unsupportedType),
            (candidate(png + Data(count: 100), type: "image/png"), .tooLarge(limits)),
        ]
        for (input, failure) in cases {
            XCTAssertThrowsError(try store.stage(input, source: .files, generation: "g1")) {
                XCTAssertEqual($0 as? ImageAttachmentFailure, failure)
            }
        }
        XCTAssertEqual(contents(of: generationDirectory(store)), [])

        // 96 encoded bytes each: the third would exceed the turn.
        let first = try store.stage(candidate(png), source: .pasteboard, generation: "g1")
        let second = try store.stage(candidate(png), source: .pasteboard, generation: "g1")
        XCTAssertThrowsError(try store.stage(candidate(png), source: .pasteboard, generation: "g1")) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .turnFull(limits))
        }
        XCTAssertEqual(contents(of: generationDirectory(store)), [first.id, second.id].sorted())
    }

    func testStagingThatCannotWriteIsUnavailable() throws {
        let blocked = root.appendingPathComponent("blocked")
        try Data("file".utf8).write(to: blocked)
        let store = ImageAttachmentStore(root: blocked.appendingPathComponent("attachments"))
        store.begin(generation: "g1")
        XCTAssertThrowsError(try store.stage(candidate(png), source: .photos, generation: "g1")) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .stagingUnavailable)
        }
        XCTAssertEqual(store.pending, [])
    }

    func testAnImageOfAnEndedSessionIsRefused() throws {
        let store = makeStore()
        XCTAssertThrowsError(try store.stage(candidate(png), source: .photos, generation: "g0")) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .sessionEnded)
        }
        store.end()
        XCTAssertThrowsError(try store.stage(candidate(png), source: .photos, generation: "g1")) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .sessionEnded)
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.root.path))
    }

    func testANewSessionSweepsEarlierOnesAndTheEndRemovesItsOwn() throws {
        let store = makeStore()
        _ = try store.stage(candidate(png), source: .photos, generation: "g1")
        let leftover = store.root.appendingPathComponent("crashed/ios-left")
        try FileManager.default.createDirectory(at: leftover.deletingLastPathComponent(),
                                                withIntermediateDirectories: true)
        try png.write(to: leftover)

        store.begin(generation: "g2")
        XCTAssertEqual(contents(of: store.root), [])
        XCTAssertEqual(store.pending, [])
        let staged = try store.stage(candidate(png), source: .files, generation: "g2")
        XCTAssertEqual(contents(of: store.root), ["g2"])

        store.remove(id: staged.id)
        XCTAssertEqual(contents(of: generationDirectory(store)), [])
        _ = try store.stage(candidate(png), source: .files, generation: "g2")
        store.end()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.root.path))
        XCTAssertEqual(store.pending, [])
    }

    func testFilenamesAreSafeBasenames() {
        XCTAssertEqual(ImageMediaType.safeFilename(nil, mediaType: "image/png"), "image.png")
        XCTAssertEqual(ImageMediaType.safeFilename("...", mediaType: "image/png"), "image.png")
        XCTAssertEqual(ImageMediaType.safeFilename("C:\\Users\\me\\cat.gif", mediaType: "image/gif"), "cat.gif")
        XCTAssertEqual(ImageMediaType.safeFilename("a\u{202E}b\u{7}.png", mediaType: "image/png"), "ab.png")
        let long = ImageMediaType.safeFilename(String(repeating: "x", count: 300), mediaType: "image/png")
        XCTAssertLessThanOrEqual(long.utf8.count, 100)
        XCTAssertTrue(long.hasSuffix(".png"))
    }

    // MARK: The guest handoff

    func testTheHandoffPublishesIntoItsGenerationAndTheNoticeCarriesNoPath() throws {
        let activeRoot = root.appendingPathComponent("active", isDirectory: true)
        let earlier = activeRoot.appendingPathComponent("data/tmp/leghost-attachments/g0/ios-old")
        try FileManager.default.createDirectory(at: earlier.deletingLastPathComponent(),
                                                withIntermediateDirectories: true)
        try png.write(to: earlier)

        let handoff = ImageAttachmentHandoff(generation: "g1", activeRoot: activeRoot)
        var adopted: [String] = []
        try handoff.prepare { adopted.append($0); return true }
        XCTAssertEqual(adopted, ["/tmp/leghost-attachments/g1"])
        XCTAssertFalse(FileManager.default.fileExists(atPath: earlier.path), "a stale generation stays adoptable")
        XCTAssertEqual(handoff.environment, "LEG_HOST_ATTACHMENT_DIR=/tmp/leghost-attachments/g1")

        XCTAssertFalse(handoff.isReceiverReady)
        try Data("1\n".utf8).write(to: handoff.hostDirectory.appendingPathComponent(".receiver-v1"))
        XCTAssertTrue(handoff.isReceiverReady)

        let store = makeStore()
        let staged = try store.stage(candidate(png, name: "IMG_0001.png", type: "image/png"),
                                     source: .photos, generation: "g1")
        let notice = try handoff.publish(store.fileURL(for: staged), attachment: staged) {
            adopted.append($0)
            return true
        }
        XCTAssertEqual(adopted.last, "/tmp/leghost-attachments/g1/\(staged.id)")
        XCTAssertEqual(try Data(contentsOf: handoff.hostDirectory.appendingPathComponent(staged.id)), png)
        XCTAssertEqual(contents(of: generationDirectory(store)), [staged.id], "the host keeps its copy")

        let text = try XCTUnwrap(String(bytes: notice, encoding: .utf8))
        let start = "\u{1b}[200~leghost-attachment/v1 "
        let end = "\u{1b}[201~"
        XCTAssertTrue(text.hasPrefix(start))
        XCTAssertTrue(text.hasSuffix(end))
        let pasted = text.dropFirst("\u{1b}[200~".count).dropLast(end.count)
        XCTAssertLessThanOrEqual(pasted.utf8.count, 2048, "the receiver refuses a longer notice")
        let json = Data(text.dropFirst(start.count).dropLast(end.count).utf8)
        let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: json) as? [String: Any])
        XCTAssertEqual(Set(fields.keys), ["generation", "id", "source", "label", "media_type", "byte_len", "sha256"])
        XCTAssertEqual(fields["generation"] as? String, "g1")
        XCTAssertEqual(fields["id"] as? String, staged.id)
        XCTAssertEqual(fields["source"] as? String, "photos")
        XCTAssertEqual(fields["label"] as? String, "IMG_0001.png")
        XCTAssertEqual(fields["media_type"] as? String, "image/png")
        XCTAssertEqual(fields["byte_len"] as? Int, png.count)
        XCTAssertEqual(fields["sha256"] as? String, pngSHA256)
        XCTAssertFalse(text.contains(root.path), "the notice names a host path")
        XCTAssertFalse(text.contains("leghost-attachments"), "the notice names a guest path")

        // A receiver that does not take the handoff gets no more notices.
        XCTAssertFalse(handoff.isTaken(staged))
        handoff.retract(staged)
        XCTAssertTrue(handoff.isTaken(staged))
        XCTAssertFalse(handoff.isReceiverReady)
        XCTAssertEqual(contents(of: generationDirectory(store)), [staged.id])

        handoff.clear()
        XCTAssertFalse(FileManager.default.fileExists(atPath: handoff.hostParent.path))
    }

    /// The notice byte for byte; `leg-tui` parses the same text in
    /// `host_attachments::tests::the_ios_host_notice_parses`.
    func testTheNoticeMatchesTheReceiverContract() throws {
        let attachment = StagedImageAttachment(
            id: "ios-00112233445566778899aabbccddeeff", generation: "0123456789abcdef", source: .photos,
            filename: "IMG_0001.png", mediaType: "image/png", byteCount: 70, sha256: pngSHA256)
        let golden = #"leghost-attachment/v1 {"byte_len":70,"generation":"0123456789abcdef","id":"ios-00112233445566778899aabbccddeeff","label":"IMG_0001.png","media_type":"image/png","sha256":"497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581","source":"photos"}"#
        XCTAssertEqual(ImageAttachmentHandoff.notice(for: attachment),
                       Array("\u{1b}[200~\(golden)\u{1b}[201~".utf8))
    }

    /// A filename a shell would expand or run crosses only as `\u` escapes.
    func testANoticeHoldsNothingAShellExpands() throws {
        let hostile = #"$(id) `x`;|&<>*?[]~!"'\猫😀.png"#
        let attachment = StagedImageAttachment(
            id: "ios-00112233445566778899aabbccddeeff", generation: "0123456789abcdef", source: .files,
            filename: hostile, mediaType: "image/png", byteCount: 70, sha256: pngSHA256)
        let notice = ImageAttachmentHandoff.notice(for: attachment)
        let pasted = try XCTUnwrap(String(bytes: notice.dropFirst(6).dropLast(6), encoding: .utf8))
        let inert = CharacterSet(charactersIn: #"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._/-{}":,\ "#)
        XCTAssertTrue(pasted.unicodeScalars.allSatisfy { inert.contains($0) }, pasted)
        XCTAssertEqual(pasted.filter { $0 == " " }.count, 1, "only the prefix ends in a space")
        let json = Data(pasted.dropFirst("leghost-attachment/v1 ".count).utf8)
        let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: json) as? [String: Any])
        XCTAssertEqual(fields["label"] as? String, hostile)
    }

    /// The image stays on the host until the receiver removes its handoff.
    @MainActor
    func testTheHostWaitsForTheReceiverToTakeAHandoff() async throws {
        let handoff = ImageAttachmentHandoff(generation: "g1", activeRoot: root.appendingPathComponent("active"))
        try handoff.prepare { _ in true }
        let store = makeStore()
        let staged = try store.stage(candidate(png), source: .photos, generation: "g1")
        _ = try handoff.publish(store.fileURL(for: staged), attachment: staged) { _ in true }
        let ignored = await handoff.waitUntilTaken(staged, timeout: 0.2)
        XCTAssertFalse(ignored)
        try FileManager.default.removeItem(at: handoff.hostDirectory.appendingPathComponent(staged.id))
        let taken = await handoff.waitUntilTaken(staged, timeout: 0.2)
        XCTAssertTrue(taken)
    }

    func testAHandoffTheGuestCannotSeeLeavesNoFile() throws {
        let handoff = ImageAttachmentHandoff(generation: "g1", activeRoot: root.appendingPathComponent("active"))
        try handoff.prepare { _ in true }
        let store = makeStore()
        let staged = try store.stage(candidate(png), source: .files, generation: "g1")
        XCTAssertThrowsError(try handoff.publish(store.fileURL(for: staged), attachment: staged) { _ in false }) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .handoffFailed)
        }
        XCTAssertEqual(contents(of: handoff.hostDirectory), [])
        XCTAssertTrue(FileManager.default.fileExists(atPath: store.fileURL(for: staged).path))

        let other = ImageAttachmentHandoff(generation: "g2", activeRoot: root.appendingPathComponent("other"))
        let foreign = try store.stage(candidate(png), source: .files, generation: "g1")
        XCTAssertThrowsError(try other.publish(store.fileURL(for: foreign), attachment: foreign) { _ in true }) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .sessionEnded)
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: store.fileURL(for: foreign).path))

        let unadoptable = ImageAttachmentHandoff(generation: "g3", activeRoot: root.appendingPathComponent("third"))
        XCTAssertThrowsError(try unadoptable.prepare { _ in false })
    }

    // MARK: Sources

    @MainActor
    func testAPickedFileIsReadInsideItsScope() async throws {
        let provider = root.appendingPathComponent("Provider", isDirectory: true)
        try FileManager.default.createDirectory(at: provider, withIntermediateDirectories: true)
        let file = provider.appendingPathComponent("cat.png")
        try png.write(to: file)
        let presenter = FixtureDocumentPresenter(importURL: file, importScopeRoot: provider)

        let read = try await ImageAttachmentFiles.read(presenter: presenter, limits: .shared)
        XCTAssertEqual(read.data, png)
        XCTAssertEqual(read.filename, "cat.png")
        XCTAssertEqual(read.declaredMediaType, "image/png")
        XCTAssertEqual(presenter.openScopes, 0, "the scope was left open")

        let small = ImageAttachmentLimits(maxEncodedBytesPerImage: 40, maxEncodedBytesPerTurn: 40)
        await assertFiles(presenter, limits: small, fail: .tooLarge(small))

        presenter.grantsAccess = false
        await assertFiles(presenter, fail: .accessRevoked)
        presenter.grantsAccess = true

        presenter.importURL = provider.appendingPathComponent("gone.png")
        await assertFiles(presenter, fail: .unreadable)

        let outside = root.appendingPathComponent("outside.png")
        try png.write(to: outside)
        presenter.importURL = outside
        await assertFiles(presenter, fail: .unreadable)

        presenter.importURL = nil
        await assertFiles(presenter, fail: .cancelled)
        XCTAssertEqual(presenter.openScopes, 0)
    }

    @MainActor
    func testThePasteboardGivesImagesAndNeverText() throws {
        let pasteboard = UIPasteboard.withUniqueName()
        defer { UIPasteboard.remove(withName: pasteboard.name) }
        pasteboard.string = "hello"
        XCTAssertThrowsError(try ImageAttachmentPasteboard.read(pasteboard)) {
            XCTAssertEqual($0 as? ImageAttachmentFailure, .noPasteboardImage)
        }
        pasteboard.setData(png, forPasteboardType: UTType.png.identifier)
        let read = try ImageAttachmentPasteboard.read(pasteboard)
        XCTAssertEqual(read.data, png)
        XCTAssertEqual(read.declaredMediaType, "image/png")
    }

    @MainActor
    func testFixtureScenariosStageOrFailWithoutAFile() async throws {
        let expected: [(FixtureImageAttachmentSources.Scenario, ImageAttachmentFailure?)] = [
            (.png, nil), (.jpeg, nil), (.mismatch, .typeMismatch), (.unsupported, .unsupportedType),
            (.oversized, .tooLarge(.shared)), (.cancel, .cancelled), (.denied, .pasteDenied),
            (.revoked, .accessRevoked), (.unreadable, .unreadable),
        ]
        for (scenario, failure) in expected {
            let store = makeStore()
            let sources = FixtureImageAttachmentSources(scenario: scenario)
            do {
                let staged = try store.stage(try await sources.pickFile(), source: .files, generation: "g1")
                XCTAssertNil(failure, "\(scenario) staged an image")
                XCTAssertEqual(staged.label, "Files · fixture-files.\(scenario == .jpeg ? "jpeg" : "png")")
            } catch {
                XCTAssertEqual(error as? ImageAttachmentFailure, failure, "\(scenario)")
                XCTAssertEqual(contents(of: generationDirectory(store)), [], "\(scenario) left a file")
            }
        }
    }

    // MARK: Helpers

    private func makeStore(limits: ImageAttachmentLimits = .shared) -> ImageAttachmentStore {
        let store = ImageAttachmentStore(root: root.appendingPathComponent("attachments", isDirectory: true),
                                         limits: limits)
        store.begin(generation: "g1")
        return store
    }

    private func candidate(_ data: Data, name: String? = "image", type: String? = nil) -> ImageAttachmentCandidate {
        ImageAttachmentCandidate(data: data, filename: name, declaredMediaType: type)
    }

    private func generationDirectory(_ store: ImageAttachmentStore) -> URL {
        store.root.appendingPathComponent(store.generation ?? "none", isDirectory: true)
    }

    private func contents(of directory: URL) -> [String] {
        ((try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []).sorted()
    }

    @MainActor
    private func assertFiles(_ presenter: FixtureDocumentPresenter, limits: ImageAttachmentLimits = .shared,
                             fail failure: ImageAttachmentFailure,
                             line: UInt = #line) async {
        do {
            _ = try await ImageAttachmentFiles.read(presenter: presenter, limits: limits)
            XCTFail("the read succeeded", line: line)
        } catch {
            XCTAssertEqual(error as? ImageAttachmentFailure, failure, line: line)
        }
    }
}
