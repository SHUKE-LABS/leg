import Foundation
import Photos

/// The access the photo library reports, independent of PhotoKit.
enum PhotoLibraryAuthorizationState: Equatable {
    case notDetermined
    case restricted
    case denied
    case authorized
    case limited

    var isGranted: Bool { self == .authorized || self == .limited }
}

/// The metadata of one video in the photo library.
struct PhotoVideoAsset: Equatable {
    let assetID: String
    let durationMs: Int64
    let createdAt: Date?
    let width: Int
    let height: Int
    /// The file name the video was imported with, when the library knows it.
    var originalFilename: String?
}

/// The photo library the Photos adapter reads.
protocol PhotoLibraryProviding: AnyObject {
    /// The current authorization, without asking the user.
    func authorizationStatus() -> PhotoLibraryAuthorizationState

    /// Asks the user when nobody asked yet, and returns the authorization
    /// that holds afterwards. A failure of the authorization API throws.
    func requestAuthorization() async throws -> PhotoLibraryAuthorizationState

    /// The videos created in the inclusive range, in no particular order.
    func fetchVideos(createdAfter: Date?, createdBefore: Date?) throws -> [PhotoVideoAsset]

    /// The video with `assetID`, with its original file name, or nil when the
    /// library holds none the app may see.
    func video(assetID: String) throws -> PhotoVideoAsset?

    /// Streams the original video resource of `assetID` to `chunk`, in
    /// order. A `chunk` that throws stops the read with that error; a
    /// cancelled task stops it as well.
    func readVideo(assetID: String, chunk: @escaping (Data) throws -> Void) async throws
}

/// The photo library of the device, through public PhotoKit API only:
/// `PHPhotoLibrary` for the authorization, `PHAsset` for the metadata, and
/// `PHAssetResourceManager` for the bytes. It never edits, deletes, or
/// shares an asset.
final class LivePhotoLibrary: PhotoLibraryProviding {
    init() {}

    func authorizationStatus() -> PhotoLibraryAuthorizationState {
        Self.state(PHPhotoLibrary.authorizationStatus(for: .readWrite)) ?? .notDetermined
    }

    func requestAuthorization() async throws -> PhotoLibraryAuthorizationState {
        let status = await PHPhotoLibrary.requestAuthorization(for: .readWrite)
        guard let state = Self.state(status) else {
            throw BridgeFailure.unavailable("authorization_failed", "Photos answered with an authorization the host does not know")
        }
        return state
    }

    func fetchVideos(createdAfter: Date?, createdBefore: Date?) throws -> [PhotoVideoAsset] {
        let options = PHFetchOptions()
        var predicates: [NSPredicate] = []
        if let createdAfter {
            predicates.append(NSPredicate(format: "creationDate >= %@", createdAfter as NSDate))
        }
        if let createdBefore {
            predicates.append(NSPredicate(format: "creationDate <= %@", createdBefore as NSDate))
        }
        if !predicates.isEmpty {
            options.predicate = NSCompoundPredicate(andPredicateWithSubpredicates: predicates)
        }
        let result = PHAsset.fetchAssets(with: .video, options: options)
        var videos: [PhotoVideoAsset] = []
        videos.reserveCapacity(result.count)
        result.enumerateObjects { asset, _, _ in
            videos.append(Self.metadata(of: asset, originalFilename: nil))
        }
        return videos
    }

    func video(assetID: String) throws -> PhotoVideoAsset? {
        guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [assetID], options: nil).firstObject,
              asset.mediaType == .video else { return nil }
        let name = Self.videoResource(of: asset)?.originalFilename
        return Self.metadata(of: asset, originalFilename: name)
    }

    func readVideo(assetID: String, chunk: @escaping (Data) throws -> Void) async throws {
        guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [assetID], options: nil).firstObject,
              let resource = Self.videoResource(of: asset) else {
            throw BridgeFailure.error("asset_not_found", "the photo library holds no video the app may read with that id")
        }
        let options = PHAssetResourceRequestOptions()
        // A video kept in iCloud is downloaded; the request deadline bounds it.
        options.isNetworkAccessAllowed = true
        let manager = PHAssetResourceManager.default()
        let read = ResourceRead()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                let requestID = manager.requestData(for: resource, options: options, dataReceivedHandler: { data in
                    guard read.failure == nil else { return }
                    do {
                        try chunk(data)
                    } catch {
                        read.fail(error)
                        if let id = read.requestID { manager.cancelDataRequest(id) }
                    }
                }, completionHandler: { error in
                    if let failure = read.failure {
                        continuation.resume(throwing: failure)
                    } else if let error {
                        continuation.resume(throwing: Self.failure(error))
                    } else {
                        continuation.resume()
                    }
                })
                if read.start(requestID) {
                    manager.cancelDataRequest(requestID)
                }
            }
        } onCancel: {
            if let id = read.cancel() { manager.cancelDataRequest(id) }
        }
    }

    /// The state of one resource request, shared by the PhotoKit callbacks
    /// and the cancellation handler.
    private final class ResourceRead: @unchecked Sendable {
        private let lock = NSLock()
        private var id: PHAssetResourceDataRequestID?
        private var error: Error?
        private var cancelled = false

        var requestID: PHAssetResourceDataRequestID? { lock.withLock { id } }
        var failure: Error? { lock.withLock { error } }

        func fail(_ failure: Error) {
            lock.withLock { if error == nil { error = failure } }
        }

        /// Records the request; true when the task was cancelled before it.
        func start(_ requestID: PHAssetResourceDataRequestID) -> Bool {
            lock.withLock {
                id = requestID
                return cancelled
            }
        }

        /// Marks the read cancelled and returns the request to cancel.
        func cancel() -> PHAssetResourceDataRequestID? {
            lock.withLock {
                cancelled = true
                if error == nil { error = BridgeFailure.cancelled }
                return id
            }
        }
    }

    private static func state(_ status: PHAuthorizationStatus) -> PhotoLibraryAuthorizationState? {
        switch status {
        case .notDetermined: return .notDetermined
        case .restricted: return .restricted
        case .denied: return .denied
        case .authorized: return .authorized
        case .limited: return .limited
        @unknown default: return nil
        }
    }

    private static func metadata(of asset: PHAsset, originalFilename: String?) -> PhotoVideoAsset {
        PhotoVideoAsset(assetID: asset.localIdentifier,
                        durationMs: Int64((asset.duration * 1000).rounded()),
                        createdAt: asset.creationDate,
                        width: asset.pixelWidth,
                        height: asset.pixelHeight,
                        originalFilename: originalFilename)
    }

    /// The original video resource of an asset.
    private static func videoResource(of asset: PHAsset) -> PHAssetResource? {
        let resources = PHAssetResource.assetResources(for: asset)
        return resources.first { $0.type == .video } ?? resources.first { $0.type == .fullSizeVideo }
    }

    private static func failure(_ error: Error) -> BridgeFailure {
        let nsError = error as NSError
        if nsError.domain == PHPhotosErrorDomain,
           nsError.code == PHPhotosError.Code.accessUserDenied.rawValue
            || nsError.code == PHPhotosError.Code.accessRestricted.rawValue {
            return .unavailable("permission_revoked", "Photos no longer grants the app access to the library")
        }
        if nsError.domain == PHPhotosErrorDomain, nsError.code == PHPhotosError.Code.userCancelled.rawValue {
            return .cancelled
        }
        return BridgeFailure.from(error)
    }
}

/// A photo library held in memory, for the tests and the DEBUG fixture of
/// the app. It counts what it was asked, so a test proves that a refused
/// request fetched no metadata and read no asset.
final class FixturePhotoLibrary: PhotoLibraryProviding {
    struct Asset {
        var metadata: PhotoVideoAsset
        var contents: Data
    }

    private let lock = NSLock()
    private var _status: PhotoLibraryAuthorizationState
    private var _metadataFetches = 0
    private var _reads: [String] = []
    private var _authorizationRequests = 0
    private var statusReads = 0

    var assets: [Asset]
    /// What a request for authorization answers; nil keeps the status, and a
    /// failure is an authorization API that failed.
    var requestOutcome: Result<PhotoLibraryAuthorizationState, Error>?
    /// Whether the access is gone after the first look at the status: the
    /// permission was granted, and the later recheck finds it revoked.
    var revokesAfterFirstCheck = false
    var chunkBytes = 64 * 1024

    init(status: PhotoLibraryAuthorizationState = .authorized, assets: [Asset] = []) {
        _status = status
        self.assets = assets
    }

    var status: PhotoLibraryAuthorizationState {
        get { lock.withLock { _status } }
        set { lock.withLock { _status = newValue } }
    }

    var metadataFetches: Int { lock.withLock { _metadataFetches } }
    /// The ids of the assets that were read, in order.
    var reads: [String] { lock.withLock { _reads } }
    var authorizationRequests: Int { lock.withLock { _authorizationRequests } }

    func authorizationStatus() -> PhotoLibraryAuthorizationState {
        lock.withLock {
            statusReads += 1
            if revokesAfterFirstCheck, statusReads > 1 { return .denied }
            return _status
        }
    }

    func requestAuthorization() async throws -> PhotoLibraryAuthorizationState {
        let outcome: Result<PhotoLibraryAuthorizationState, Error>? = lock.withLock {
            _authorizationRequests += 1
            return requestOutcome
        }
        guard let outcome else { return status }
        let state = try outcome.get()
        status = state
        return state
    }

    func fetchVideos(createdAfter: Date?, createdBefore: Date?) throws -> [PhotoVideoAsset] {
        lock.withLock { _metadataFetches += 1 }
        return assets.map(\.metadata).filter { video in
            guard let created = video.createdAt else { return createdAfter == nil && createdBefore == nil }
            return (createdAfter.map { created >= $0 } ?? true) && (createdBefore.map { created <= $0 } ?? true)
        }
    }

    func video(assetID: String) throws -> PhotoVideoAsset? {
        lock.withLock { _metadataFetches += 1 }
        return assets.first { $0.metadata.assetID == assetID }?.metadata
    }

    func readVideo(assetID: String, chunk: @escaping (Data) throws -> Void) async throws {
        lock.withLock { _reads.append(assetID) }
        guard let asset = assets.first(where: { $0.metadata.assetID == assetID }) else {
            throw BridgeFailure.error("asset_not_found", "the photo library holds no video with that id")
        }
        var offset = 0
        while offset < asset.contents.count {
            try Task.checkCancellation()
            let end = min(offset + chunkBytes, asset.contents.count)
            try chunk(asset.contents.subdata(in: offset..<end))
            offset = end
            await Task.yield()
        }
    }
}

/// Serves `photos.search` and `photos.export`.
///
/// Both operations authorize before they touch the library, and both carry
/// the authorization in a successful result:
///
/// | Authorization | Status | Reason / result |
/// | --- | --- | --- |
/// | authorized | `ok` | `authorization: "full"` |
/// | limited | `ok` | `authorization: "limited"` |
/// | denied after the request | `denied` | `user_refused` |
/// | restricted | `unavailable` | `permission_restricted` |
/// | granted, then gone at the recheck | `unavailable` | `permission_revoked` |
/// | still not determined after the request, or the API failed | `unavailable` | `authorization_failed` |
///
/// A refused row fetches no metadata and reads no asset.
final class PhotosAdapter: BridgeCapabilityAdapter {
    private let library: PhotoLibraryProviding
    private let presenter: BridgeDocumentPresenter
    /// The transfer bound; the tests lower it.
    var transferLimit = BridgeLimits.maxTransferBytes
    /// Runs between the preflight of a destination and its commit.
    var beforeCommit: ((URL) -> Void)?
    /// Whether this adapter saw the access granted, so a later denial is a
    /// revocation rather than a refusal.
    private var grantedBefore = false

    init(library: PhotoLibraryProviding, presenter: BridgeDocumentPresenter) {
        self.library = library
        self.presenter = presenter
    }

    func preview(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON {
        guard case .photosExport(let input) = request else {
            throw BridgeFailure.protocolViolation("\(request.capability) runs without an approval")
        }
        // The summary is made of the input alone: a preview neither asks for
        // the authorization nor looks into the library.
        let place = input.destination == .guestWorkspace
            ? "into the guest workspace" : "into a folder you pick in Files"
        var members: [BridgeJSONMember] = [
            BridgeJSONMember(key: "action", value: .string("photos.export")),
            BridgeJSONMember(key: "summary", value: .string("Copy the video \(input.assetID) from Photos \(place)")),
            BridgeJSONMember(key: "asset_id", value: .string(input.assetID)),
            BridgeJSONMember(key: "destination", value: .string(input.destination.rawValue)),
        ]
        if let filename = input.filename {
            members.append(BridgeJSONMember(key: "filename", value: .string(filename)))
        }
        return .object(members)
    }

    func perform(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON {
        switch request {
        case .photosSearch(let input):
            return try await search(input)
        case .photosExport(let input):
            return try await export(input, context: context)
        default:
            throw BridgeFailure.unavailable("capability_unsupported", "the Photos adapter does not serve \(request.capability)")
        }
    }

    private func search(_ input: PhotosSearchInput) async throws -> BridgeJSON {
        let authorization = try await authorize()
        let videos: [PhotoVideoAsset]
        do {
            videos = try library.fetchVideos(createdAfter: input.createdAfter, createdBefore: input.createdBefore)
        } catch {
            throw BridgeFailure.from(error)
        }
        let matches = Self.sorted(videos.filter { Self.isReportable($0, input) }, by: input.sort)
        let page = matches.prefix(input.limit)
        let assets: [BridgeJSON] = page.map { video in
            .object([
                "asset_id": .string(video.assetID),
                "media_type": .string("video"),
                "duration_ms": .integer(video.durationMs),
                "created_at": .string(BridgeValidation.formatRFC3339(video.createdAt!)),
                "width": .integer(video.width),
                "height": .integer(video.height),
            ])
        }
        return .object([
            "authorization": .string(authorization.rawValue),
            "assets": .array(assets),
            "truncated": .bool(matches.count > input.limit),
        ])
    }

    private func export(_ input: PhotosExportInput, context: BridgeOperationContext) async throws -> BridgeJSON {
        let authorization = try await authorize()
        let video: PhotoVideoAsset?
        do {
            video = try library.video(assetID: input.assetID)
        } catch {
            throw BridgeFailure.from(error)
        }
        guard let video else {
            throw BridgeFailure.error("asset_not_found", "the photo library holds no video the app may read with that id")
        }
        let copyID = BridgeValidation.newGeneratedID()
        let name = input.filename ?? Self.defaultName(video.originalFilename, copyID: copyID)
        var directory: URL?
        if input.destination == .filesPicker {
            let picked = try await presenter.pickExportDirectory()
            guard presenter.startAccessing(picked) else {
                throw BridgeFailure.unavailable("permission_revoked", "the Files provider no longer grants the picked folder")
            }
            directory = picked
        }
        defer { if let directory { presenter.stopAccessing(directory) } }

        let transfer = try BridgeTransfer(staging: context.paths.staging, limit: transferLimit)
        defer { transfer.discard() }
        do {
            try await library.readVideo(assetID: video.assetID) { data in
                try transfer.append(data)
            }
        } catch {
            throw BridgeFailure.from(error)
        }
        try Task.checkCancellation()
        let managedRoot = context.paths.managedRoot
        let beforeCommit = beforeCommit
        let staged: BridgeStagedFile = try await BridgeIO.run(deadline: context.deadline) { flag in
            let staged = try transfer.finish()
            try flag.check()
            if let directory {
                try BridgeCommit.commitToDirectory(staged, directory: directory, name: name,
                                                   operationID: context.operationID, flag: flag,
                                                   beforeCommit: beforeCommit)
            } else {
                try BridgeCommit.commitToManagedRoot(staged, managedRoot: managedRoot, copyID: copyID,
                                                     name: name, beforeCommit: beforeCommit)
            }
            return staged
        }
        return .object([
            "authorization": .string(authorization.rawValue),
            "copy_id": .string(copyID),
            "relative_path": directory == nil ? .string("\(copyID)/\(name)") : .null,
            "name": .string(name),
            "media_type": .string("video"),
            "size_bytes": .integer(staged.size),
            "sha256": .string(staged.sha256),
        ])
    }

    /// Asks for the authorization when it is not granted, and checks it once
    /// more right before the library is touched.
    private func authorize() async throws -> BridgePhotoAuthorization {
        let initial = library.authorizationStatus()
        var state = initial
        if !initial.isGranted {
            if initial == .denied, grantedBefore {
                throw Self.revoked
            }
            do {
                state = try await library.requestAuthorization()
            } catch {
                throw BridgeFailure.unavailable("authorization_failed", "the Photos authorization failed")
            }
        }
        switch state {
        case .authorized, .limited:
            break
        case .denied:
            throw BridgeFailure.denied("user_refused", "the user did not allow access to Photos")
        case .restricted:
            throw BridgeFailure.unavailable("permission_restricted", "access to Photos is restricted on this device")
        case .notDetermined:
            throw BridgeFailure.unavailable("authorization_failed", "the Photos authorization was not decided")
        }
        let recheck = library.authorizationStatus()
        guard recheck.isGranted else { throw Self.revoked }
        grantedBefore = true
        return recheck == .authorized ? .full : .limited
    }

    private static let revoked = BridgeFailure.unavailable(
        "permission_revoked", "the access to Photos was granted, and it is gone now")

    /// Whether a video can be reported within the result bounds and matches
    /// the date range. A video outside the bounds — an id that is not
    /// printable or too long, no creation date, a dimension outside
    /// 1…32768, a duration outside 0…604800000 ms — is left out of the
    /// matches rather than failing the whole search.
    private static func isReportable(_ video: PhotoVideoAsset, _ input: PhotosSearchInput) -> Bool {
        guard BridgeValidation.isPrintableID(video.assetID, maxBytes: BridgeLimits.maxAssetIDBytes),
              let created = video.createdAt,
              (1...BridgeLimits.maxDimension).contains(video.width),
              (1...BridgeLimits.maxDimension).contains(video.height),
              (0...BridgeLimits.maxDurationMs).contains(video.durationMs) else { return false }
        if let after = input.createdAfter, created < after { return false }
        if let before = input.createdBefore, created > before { return false }
        return true
    }

    /// The deterministic order: the sort key descending, then the asset id
    /// ascending by its UTF-8 bytes.
    static func sorted(_ videos: [PhotoVideoAsset], by sort: BridgePhotoSort) -> [PhotoVideoAsset] {
        videos.sorted { left, right in
            switch sort {
            case .durationDesc where left.durationMs != right.durationMs:
                return left.durationMs > right.durationMs
            case .createdDesc where left.createdAt != right.createdAt:
                return (left.createdAt ?? .distantPast) > (right.createdAt ?? .distantPast)
            default:
                return left.assetID.utf8.lexicographicallyPrecedes(right.assetID.utf8)
            }
        }
    }

    /// The original file name when it is safe to write as is, else a name
    /// made of the copy id.
    static func defaultName(_ original: String?, copyID: String) -> String {
        if let original, BridgeValidation.isSafeBasename(original) { return original }
        return "video-\(copyID.prefix(16)).mov"
    }
}
