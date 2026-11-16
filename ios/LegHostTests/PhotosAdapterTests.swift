import XCTest
@testable import LegHost

/// The Photos adapter: the authorization table, the metadata-only search,
/// and the export of one approved video.
final class PhotosAdapterTests: XCTestCase {
    private var scratch: BridgeAdapterScratch!
    private var presenter: FixtureDocumentPresenter!

    override func setUpWithError() throws {
        scratch = try BridgeAdapterScratch()
        presenter = FixtureDocumentPresenter()
    }

    override func tearDown() {
        presenter = nil
        scratch = nil
    }

    private static let epoch = Date(timeIntervalSince1970: 1_700_000_000)

    private static func asset(_ id: String, durationMs: Int64 = 1_000, createdOffset: TimeInterval = 0,
                              width: Int = 1920, height: Int = 1080, name: String? = nil,
                              contents: String = "video") -> FixturePhotoLibrary.Asset {
        FixturePhotoLibrary.Asset(
            metadata: PhotoVideoAsset(assetID: id, durationMs: durationMs,
                                      createdAt: epoch.addingTimeInterval(createdOffset),
                                      width: width, height: height, originalFilename: name),
            contents: Data(contents.utf8))
    }

    private static func search(sort: BridgePhotoSort = .durationDesc, limit: Int = 20,
                               after: Date? = nil, before: Date? = nil) -> BridgeCapabilityRequest {
        .photosSearch(PhotosSearchInput(createdAfter: after, createdBefore: before, sort: sort, limit: limit))
    }

    private static func export(_ id: String, to destination: BridgePhotoDestination = .guestWorkspace,
                               filename: String? = nil) -> BridgeCapabilityRequest {
        .photosExport(PhotosExportInput(assetID: id, destination: destination, filename: filename))
    }

    private func run(_ request: BridgeCapabilityRequest, on adapter: PhotosAdapter) async -> Result<BridgeJSON, BridgeFailure> {
        await bridgeOutcome { try await adapter.perform(request, context: scratch.context) }
    }

    private func ids(_ result: Result<BridgeJSON, BridgeFailure>) -> [String] {
        guard case .array(let assets)? = result.value?["assets"] else { return [] }
        return assets.compactMap { $0["asset_id"]?.stringValue }
    }

    // MARK: - The authorization table

    func testTheAuthorizationTableHoldsForBothOperations() async throws {
        struct Row {
            let name: String
            let status: PhotoLibraryAuthorizationState
            var request: Result<PhotoLibraryAuthorizationState, Error>?
            var revokes = false
            let expected: Result<String, BridgeFailure>
        }
        struct APIFailure: Error {}
        let rows = [
            Row(name: "authorized", status: .authorized, expected: .success("full")),
            Row(name: "limited", status: .limited, expected: .success("limited")),
            Row(name: "granted on request", status: .notDetermined, request: .success(.authorized), expected: .success("full")),
            Row(name: "denied after the request", status: .notDetermined, request: .success(.denied),
                expected: .failure(.denied("user_refused", ""))),
            Row(name: "denied before", status: .denied, expected: .failure(.denied("user_refused", ""))),
            Row(name: "restricted", status: .restricted, expected: .failure(.unavailable("permission_restricted", ""))),
            Row(name: "revoked", status: .authorized, revokes: true,
                expected: .failure(.unavailable("permission_revoked", ""))),
            Row(name: "still not determined", status: .notDetermined,
                expected: .failure(.unavailable("authorization_failed", ""))),
            Row(name: "API failure", status: .notDetermined, request: .failure(APIFailure()),
                expected: .failure(.unavailable("authorization_failed", ""))),
        ]
        for row in rows {
            for request in [Self.search(), Self.export("a1")] {
                let library = FixturePhotoLibrary(status: row.status, assets: [Self.asset("a1")])
                library.requestOutcome = row.request
                library.revokesAfterFirstCheck = row.revokes
                let adapter = PhotosAdapter(library: library, presenter: presenter)

                let result = await run(request, on: adapter)

                let label = "\(row.name) / \(request.capability)"
                switch row.expected {
                case .success(let authorization):
                    XCTAssertEqual(result.value?["authorization"]?.stringValue, authorization, label)
                case .failure(let failure):
                    XCTAssertNil(result.value, label)
                    XCTAssertEqual(result.failure?.status, failure.status, label)
                    XCTAssertEqual(result.failure?.reason, failure.reason, label)
                    XCTAssertEqual(library.metadataFetches, 0, "\(label) fetched metadata")
                    XCTAssertEqual(library.reads, [], "\(label) read an asset")
                    XCTAssertTrue(scratch.managedRootIsEmpty, label)
                    XCTAssertFalse(scratch.exists(scratch.staging), label)
                }
                try? FileManager.default.removeItem(at: scratch.managedRoot)
            }
        }
    }

    func testAGrantThatIsLaterDeniedIsARevocation() async {
        let library = FixturePhotoLibrary(status: .authorized, assets: [Self.asset("a1")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)
        let first = await run(Self.search(), on: adapter)
        XCTAssertNotNil(first.value)
        library.status = .denied

        let result = await run(Self.search(), on: adapter)

        XCTAssertEqual(result.failure?.reason, "permission_revoked")
        XCTAssertEqual(library.metadataFetches, 1)
    }

    // MARK: - Search

    func testASearchSortsByDurationThenAssetID() async {
        let library = FixturePhotoLibrary(assets: [
            Self.asset("b", durationMs: 5_000), Self.asset("c", durationMs: 9_000),
            Self.asset("a", durationMs: 5_000), Self.asset("d", durationMs: 1_000),
        ])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.search(), on: adapter)

        XCTAssertEqual(ids(result), ["c", "a", "b", "d"])
        XCTAssertEqual(result.value?["truncated"], .bool(false))
    }

    func testASearchSortsByCreationThenAssetID() async {
        let library = FixturePhotoLibrary(assets: [
            Self.asset("x", createdOffset: 10), Self.asset("z", createdOffset: 20), Self.asset("y", createdOffset: 10),
        ])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.search(sort: .createdDesc), on: adapter)

        XCTAssertEqual(ids(result), ["z", "x", "y"])
    }

    func testASearchReturnsExactlyTheResultSchema() async throws {
        let library = FixturePhotoLibrary(assets: [Self.asset("a1", durationMs: 1_500, width: 640, height: 480)])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.search(), on: adapter)

        let value = try XCTUnwrap(result.value)

        XCTAssertEqual(value.keys, ["authorization", "assets", "truncated"])
        guard case .array(let assets)? = value["assets"] else { return XCTFail("no assets") }
        XCTAssertEqual(assets.first?.keys, ["asset_id", "media_type", "duration_ms", "created_at", "width", "height"])
        XCTAssertEqual(assets.first?["media_type"]?.stringValue, "video")
        XCTAssertEqual(assets.first?["duration_ms"]?.integerValue, 1_500)
        XCTAssertEqual(assets.first?["created_at"]?.stringValue, "2023-11-14T22:13:20Z")
        XCTAssertEqual(assets.first?["width"]?.integerValue, 640)
        XCTAssertEqual(library.reads, [], "a search read an asset")
    }

    func testAOneItemLimitReportsTheTruncation() async {
        let library = FixturePhotoLibrary(assets: [Self.asset("a", durationMs: 1), Self.asset("b", durationMs: 2)])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.search(limit: 1), on: adapter)

        XCTAssertEqual(ids(result), ["b"])
        XCTAssertEqual(result.value?["truncated"], .bool(true))
    }

    func testAFullPageIsNotTruncated() async {
        let assets = (0..<100).map { Self.asset(String(format: "id-%03d", $0), durationMs: Int64($0)) }
        let adapter = PhotosAdapter(library: FixturePhotoLibrary(assets: assets), presenter: presenter)

        let result = await run(Self.search(limit: 100), on: adapter)

        XCTAssertEqual(ids(result).count, 100)
        XCTAssertEqual(ids(result).first, "id-099")
        XCTAssertEqual(result.value?["truncated"], .bool(false))
    }

    func testAPageWithMoreMatchesIsTruncated() async {
        let assets = (0..<30).map { Self.asset(String(format: "id-%03d", $0), durationMs: 7) }
        let adapter = PhotosAdapter(library: FixturePhotoLibrary(assets: assets), presenter: presenter)

        let result = await run(Self.search(limit: 20), on: adapter)

        XCTAssertEqual(ids(result), (0..<20).map { String(format: "id-%03d", $0) })
        XCTAssertEqual(result.value?["truncated"], .bool(true))
    }

    func testASearchKeepsTheInclusiveDateRangeAndSkipsUnreportableVideos() async {
        let library = FixturePhotoLibrary(assets: [
            Self.asset("early", createdOffset: -1), Self.asset("start", createdOffset: 0),
            Self.asset("end", createdOffset: 100), Self.asset("late", createdOffset: 101),
            Self.asset("huge", createdOffset: 50, width: 40_000),
            Self.asset("long", durationMs: 604_800_001, createdOffset: 50),
            Self.asset("bad\u{07}id", createdOffset: 50),
        ])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.search(after: Self.epoch, before: Self.epoch.addingTimeInterval(100)), on: adapter)

        XCTAssertEqual(Set(ids(result)), ["start", "end"])
    }

    // MARK: - Export

    func testAnExportPreviewReadsNothing() async throws {
        let library = FixturePhotoLibrary(status: .notDetermined, assets: [Self.asset("a1")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let review = try await adapter.preview(Self.export("a1"), context: scratch.context)

        XCTAssertEqual(review["asset_id"]?.stringValue, "a1")
        XCTAssertEqual(library.authorizationRequests, 0)
        XCTAssertEqual(library.metadataFetches, 0)
        XCTAssertEqual(library.reads, [])
        XCTAssertEqual(presenter.picks, 0)
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnExportCopiesOnlyTheSelectedVideoIntoTheManagedLayout() async throws {
        let library = FixturePhotoLibrary(status: .limited, assets: [
            Self.asset("a1", name: "IMG_0001.MOV", contents: "first video"), Self.asset("a2", contents: "second"),
        ])
        library.chunkBytes = 4
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.export("a1"), on: adapter)

        let value = try XCTUnwrap(result.value, "\(String(describing: result.failure))")

        XCTAssertEqual(value.keys, ["authorization", "copy_id", "relative_path", "name", "media_type", "size_bytes", "sha256"])
        let copyID = try XCTUnwrap(value["copy_id"]?.stringValue)
        XCTAssertEqual(value["authorization"]?.stringValue, "limited")
        XCTAssertEqual(value["name"]?.stringValue, "IMG_0001.MOV")
        XCTAssertEqual(value["relative_path"]?.stringValue, "\(copyID)/IMG_0001.MOV")
        XCTAssertEqual(value["size_bytes"]?.integerValue, 11)
        XCTAssertEqual(value["sha256"]?.stringValue, BridgeCanonicalJSON.sha256Hex(Data("first video".utf8)))
        XCTAssertEqual(try String(contentsOf: scratch.managedRoot.appendingPathComponent("\(copyID)/IMG_0001.MOV")),
                       "first video")
        XCTAssertEqual(scratch.published, [copyID], "the guest was not given the copy")
        XCTAssertEqual(library.reads, ["a1"])
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnExportToFilesHasNoRelativePath() async throws {
        presenter.exportDirectory = scratch.picked
        let library = FixturePhotoLibrary(assets: [Self.asset("a1", name: "../evil.mov")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.export("a1", to: .filesPicker, filename: "clip.mov"), on: adapter)

        let value = try XCTUnwrap(result.value, "\(String(describing: result.failure))")

        XCTAssertEqual(value["relative_path"], .null)
        XCTAssertEqual(value["name"]?.stringValue, "clip.mov")
        XCTAssertEqual(scratch.contents(of: scratch.picked), ["clip.mov"])
        XCTAssertTrue(scratch.managedRootIsEmpty)
    }

    func testAnUnsafeOriginalNameIsReplaced() {
        XCTAssertEqual(PhotosAdapter.defaultName("../evil.mov", copyID: "0123456789abcdef0123"), "video-0123456789abcdef.mov")
        XCTAssertEqual(PhotosAdapter.defaultName(nil, copyID: "abc"), "video-abc.mov")
        XCTAssertEqual(PhotosAdapter.defaultName("clip.mov", copyID: "abc"), "clip.mov")
    }

    func testAnOversizedVideoLeavesNoOutput() async {
        let library = FixturePhotoLibrary(assets: [Self.asset("a1", contents: String(repeating: "x", count: 100))])
        library.chunkBytes = 8
        let adapter = PhotosAdapter(library: library, presenter: presenter)
        adapter.transferLimit = 20

        let result = await run(Self.export("a1"), on: adapter)

        XCTAssertEqual(result.failure?.status, .error)
        XCTAssertEqual(result.failure?.reason, "size_limit")
        XCTAssertTrue(scratch.managedRootIsEmpty)
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testAnUnknownAssetIsNotFound() async {
        let library = FixturePhotoLibrary(assets: [Self.asset("a1")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.export("missing"), on: adapter)

        XCTAssertEqual(result.failure?.reason, "asset_not_found")
        XCTAssertEqual(library.reads, [])
    }

    func testAClosedFolderPickerIsACancellationBeforeAnyRead() async {
        presenter.exportDirectory = nil
        let library = FixturePhotoLibrary(assets: [Self.asset("a1")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.export("a1", to: .filesPicker), on: adapter)

        XCTAssertEqual(result.failure?.status, .cancelled)
        XCTAssertEqual(result.failure?.reason, "cancelled")
        XCTAssertEqual(library.reads, [])
    }

    func testARevokedFolderScopeIsUnavailable() async {
        presenter.exportDirectory = scratch.picked
        presenter.grantsAccess = false
        let library = FixturePhotoLibrary(assets: [Self.asset("a1")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)

        let result = await run(Self.export("a1", to: .filesPicker), on: adapter)

        XCTAssertEqual(result.failure?.reason, "permission_revoked")
        XCTAssertEqual(library.reads, [])
        XCTAssertEqual(scratch.contents(of: scratch.picked), [])
    }

    func testAFilesDestinationThatAppearsBeforeTheCommitIsNotReplaced() async throws {
        presenter.exportDirectory = scratch.picked
        let library = FixturePhotoLibrary(assets: [Self.asset("a1", contents: "new")])
        let adapter = PhotosAdapter(library: library, presenter: presenter)
        adapter.beforeCommit = { destination in
            try? Data("old".utf8).write(to: destination)
        }

        let result = await run(Self.export("a1", to: .filesPicker, filename: "clip.mov"), on: adapter)

        XCTAssertEqual(result.failure?.status, .denied)
        XCTAssertEqual(result.failure?.reason, "duplicate_destination")
        XCTAssertEqual(try String(contentsOf: scratch.picked.appendingPathComponent("clip.mov")), "old")
        XCTAssertEqual(scratch.contents(of: scratch.picked), ["clip.mov"])
        XCTAssertFalse(scratch.exists(scratch.staging))
    }

    func testACancelledExportLeavesNoOutput() async {
        let library = FixturePhotoLibrary(assets: [Self.asset("a1", contents: String(repeating: "x", count: 4_096))])
        library.chunkBytes = 1
        let adapter = PhotosAdapter(library: library, presenter: presenter)
        let context = scratch.context

        let task = Task { await bridgeOutcome { try await adapter.perform(Self.export("a1"), context: context) } }
        task.cancel()
        let result = await task.value

        XCTAssertEqual(result.failure?.status, .cancelled)
        XCTAssertTrue(scratch.managedRootIsEmpty)
        XCTAssertFalse(scratch.exists(scratch.staging))
    }
}
