import Darwin
import XCTest
@testable import LegHost

/// The host side of the capability bridge.
///
/// The listener runs for real, on a real Darwin socket; the test is the
/// guest. The endpoint registrar and the adapters are fixtures, so the test
/// sees every mapping and every piece of native work the listener starts.
final class BridgeListenerTests: XCTestCase {
    // MARK: Fixtures

    /// Records the mappings the listener asks for.
    private final class FakeRegistrar: BridgeEndpointRegistrar {
        var status: Int32 = 0
        private(set) var events: [String] = []

        func register(generation: UInt64, path: String) -> Int32 {
            events.append("register \(generation)")
            return status
        }

        func unregister(generation: UInt64) {
            events.append("unregister \(generation)")
        }
    }

    /// An adapter that counts its work and answers what the test says.
    private final class FixtureAdapter: BridgeCapabilityAdapter, @unchecked Sendable {
        private let lock = NSLock()
        private var _previews = 0
        private var _performs = 0
        private var _cancelled = 0
        var previewResult: () async throws -> BridgeJSON = { .object(["summary": .string("export one video")]) }
        var performResult: () async throws -> BridgeJSON = { .object(["done": .bool(true)]) }
        /// The managed copy a perform commits and gives the guest.
        var publishedCopy: String?

        var previews: Int { lock.withLock { _previews } }
        var performs: Int { lock.withLock { _performs } }
        var cancelled: Int { lock.withLock { _cancelled } }

        func preview(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON {
            lock.withLock { _previews += 1 }
            return try await previewResult()
        }

        func perform(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON {
            lock.withLock { _performs += 1 }
            do {
                if let publishedCopy { try context.publishManagedCopy(publishedCopy) }
                return try await performResult()
            } catch is CancellationError {
                lock.withLock { _cancelled += 1 }
                throw CancellationError()
            }
        }
    }

    /// A gate an adapter that ignores cancellation waits at.
    private final class Gate: @unchecked Sendable {
        private let lock = NSLock()
        private var opened = false
        var isOpen: Bool { lock.withLock { opened } }
        func open() { lock.withLock { opened = true } }
    }

    /// Work that runs until it is cancelled.
    private static func forever() async throws -> BridgeJSON {
        while true {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
    }

    private var root: URL!
    private var registrar: FakeRegistrar!
    private var adapter: FixtureAdapter!
    private var listener: BridgeListener!
    /// The guest paths the listener gave the guest, and whether it can.
    private let adoptions = Adoptions()

    private final class Adoptions: @unchecked Sendable {
        private let lock = NSLock()
        private var _paths: [String] = []
        var succeeds = true
        var paths: [String] { lock.withLock { _paths } }

        func adopt(_ path: String) -> Bool {
            lock.withLock {
                _paths.append(path)
                return succeeds
            }
        }
    }

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("bridge-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        registrar = FakeRegistrar()
        adapter = FixtureAdapter()
        listener = makeListener(generation: 7)
        try listener.start()
    }

    override func tearDownWithError() throws {
        listener?.beginShutdown()
        listener?.finishShutdown()
        listener = nil
        try? FileManager.default.removeItem(at: root)
    }

    private func makeListener(generation: UInt64) -> BridgeListener {
        let adapters: [String: BridgeCapabilityAdapter] = Dictionary(
            uniqueKeysWithValues: BridgeCapabilityRequest.capabilities.map { ($0, adapter as BridgeCapabilityAdapter) })
        return BridgeListener(generation: generation, adapters: adapters, registrar: registrar,
                              stagingRoot: root.appendingPathComponent("staging"),
                              managedRoot: root.appendingPathComponent("managed"),
                              guestAdopter: adoptions.adopt)
    }

    // MARK: The guest side

    private static let exportInput: BridgeJSON = .object(["asset_id": .string("A1"), "destination": .string("guest_workspace")])
    private static let reorderedExportInput: BridgeJSON = .object(["destination": .string("guest_workspace"), "asset_id": .string("A1")])
    private static let searchInput: BridgeJSON = .object([
        "filter": .object(["media_type": .string("video")]), "sort": .string("duration_desc"),
    ])

    private func frame(_ capability: String, _ input: BridgeJSON, token: String? = nil, timeoutMs: Int = 5_000,
                       requestID: String = UUID().uuidString, digest: String? = nil) -> Data {
        var members: [BridgeJSONMember] = [
            BridgeJSONMember(key: "version", value: .integer(1)),
            BridgeJSONMember(key: "request_id", value: .string(requestID)),
            BridgeJSONMember(key: "nonce", value: .string("n-" + requestID)),
            BridgeJSONMember(key: "capability", value: .string(capability)),
            BridgeJSONMember(key: "input", value: input),
            BridgeJSONMember(key: "input_sha256", value: .string(digest ?? BridgeCanonicalJSON.digest(input))),
            BridgeJSONMember(key: "timeout_ms", value: .integer(timeoutMs)),
        ]
        if let token {
            members.append(BridgeJSONMember(key: "approval_token", value: .string(token)))
        }
        return BridgeCanonicalJSON.encode(.object(members))
    }

    /// Connects to the listener, sends `line`, and reads the reply line;
    /// nil when the listener closed the connection without one.
    private func exchange(_ line: Data, on target: BridgeListener? = nil,
                          closeAfter: TimeInterval? = nil) throws -> BridgeJSON? {
        let fd = try connect(to: (target ?? listener).socketPath)
        defer { close(fd) }
        var bytes = Array(line)
        bytes.append(0x0A)
        // The listener may close an oversized frame before it is all sent.
        var offset = 0
        while offset < bytes.count {
            let written = bytes[offset...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            guard written > 0 else { break }
            offset += written
        }
        if let closeAfter {
            Thread.sleep(forTimeInterval: closeAfter)
            return nil
        }
        var reply = Data()
        var chunk = [UInt8](repeating: 0, count: 4096)
        let deadline = Date().addingTimeInterval(15)
        while Date() < deadline {
            var pollFD = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            guard poll(&pollFD, 1, 200) > 0 else { continue }
            let count = read(fd, &chunk, chunk.count)
            if count <= 0 { break }
            reply.append(contentsOf: chunk[0..<count])
            if let newline = reply.firstIndex(of: 0x0A) {
                return try BridgeCanonicalJSON.parse(reply[reply.startIndex..<newline])
            }
        }
        XCTAssertTrue(reply.isEmpty, "a partial reply")
        return nil
    }

    private func connect(to path: String) throws -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw POSIXError(.EIO) }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            let bytes = Array(path.utf8)
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let status = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard status == 0 else {
            close(fd)
            throw POSIXError(.ECONNREFUSED)
        }
        return fd
    }

    /// Binds a socket of the test at `path`, as another process would.
    private func bindSocket(at path: String) throws -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw POSIXError(.EIO) }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            let bytes = Array(path.utf8)
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let status = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard status == 0 else {
            close(fd)
            throw POSIXError(.EADDRINUSE)
        }
        return fd
    }

    private func status(_ reply: BridgeJSON?) -> String? { reply?["status"]?.stringValue }
    private func reason(_ reply: BridgeJSON?) -> String? { reply?["error"]?["reason"]?.stringValue }

    private func preview(_ input: BridgeJSON = BridgeListenerTests.exportInput) throws -> String {
        let reply = try exchange(frame("photos.export", input))
        XCTAssertEqual(status(reply), "approval_required")
        return try XCTUnwrap(reply?["result"]?["approval_token"]?.stringValue)
    }

    // MARK: Start and stop

    func testStartMapsTheGenerationAndStopRemovesEverything() throws {
        let path = listener.socketPath
        XCTAssertLessThanOrEqual(path.utf8.count, Int(ISH_BRIDGE_SOCKET_HOST_PATH_MAX))
        var info = stat()
        XCTAssertEqual(lstat(path, &info), 0)
        XCTAssertEqual(info.st_mode & S_IFMT, S_IFSOCK)
        XCTAssertEqual(info.st_mode & 0o777, 0o600)
        XCTAssertEqual(registrar.events, ["register 7"])

        _ = try preview()
        listener.beginShutdown()
        listener.finishShutdown()
        XCTAssertEqual(registrar.events, ["register 7", "unregister 7"])
        XCTAssertNotEqual(lstat(path, &info), 0)
        XCTAssertNotEqual(lstat((path as NSString).deletingLastPathComponent, &info), 0)
        XCTAssertTrue(listener.approvals.records.isEmpty)
        XCTAssertThrowsError(try connect(to: path))
    }

    func testAFailedMappingFailsTheStartAndLeavesNothing() throws {
        registrar.status = Int32(ISH_BRIDGE_SOCKET_BUSY)
        let second = makeListener(generation: 8)
        XCTAssertThrowsError(try second.start()) { error in
            XCTAssertTrue((error as? BridgeListenerError)?.errorDescription?.hasPrefix("IOS_BRIDGE_LISTENER_ERROR") == true)
        }
        var info = stat()
        XCTAssertNotEqual(lstat(second.socketPath, &info), 0)
        XCTAssertFalse(registrar.events.contains("unregister 8"))
    }

    func testASocketThatTakesThePathBeforeTheBindIsNotRemoved() throws {
        let second = makeListener(generation: 8)
        var foreign: Int32 = -1
        second.beforeBind = { path in foreign = (try? self.bindSocket(at: path)) ?? -1 }
        XCTAssertThrowsError(try second.start())
        XCTAssertGreaterThanOrEqual(foreign, 0, "the test could not take the path")
        defer {
            close(foreign)
            unlink(second.socketPath)
            rmdir((second.socketPath as NSString).deletingLastPathComponent)
        }
        var info = stat()
        XCTAssertEqual(lstat(second.socketPath, &info), 0, "the listener removed a socket it did not bind")
        XCTAssertEqual(info.st_mode & S_IFMT, S_IFSOCK)
        XCTAssertFalse(registrar.events.contains("register 8"))
    }

    func testTeardownWaitsForAnAdapterThatDoesNotStop() throws {
        let token = try preview()
        let gate = Gate()
        adapter.performResult = {
            // Ignores the cancellation.
            while !gate.isOpen { usleep(10_000) }
            return .object(["done": .bool(true)])
        }
        let staging = root.appendingPathComponent("staging").appendingPathComponent(listener.generationID)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let path = listener.socketPath
        let replied = expectation(description: "reply")
        DispatchQueue.global().async {
            _ = try? self.exchange(self.frame("photos.export", Self.exportInput, token: token))
            replied.fulfill()
        }
        let deadline = Date().addingTimeInterval(5)
        while adapter.performs == 0 && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        listener.joinTimeout = .milliseconds(200)
        listener.cancelGrace = .milliseconds(100)
        listener.beginShutdown()
        wait(for: [replied], timeout: 15)
        // The connection worker has answered and ended by now; only the work
        // of the adapter is left.
        Thread.sleep(forTimeInterval: 0.5)
        XCTAssertFalse(listener.finishShutdown(), "the teardown did not wait for the adapter")
        var info = stat()
        XCTAssertEqual(registrar.events.last, "register 7", "the mapping went while the adapter ran")
        XCTAssertTrue(FileManager.default.fileExists(atPath: staging.path), "the staging went while the adapter ran")
        XCTAssertEqual(lstat(path, &info), 0)

        gate.open()
        let released = Date().addingTimeInterval(5)
        while registrar.events.last != "unregister 7" && Date() < released {
            Thread.sleep(forTimeInterval: 0.05)
        }
        XCTAssertEqual(registrar.events.last, "unregister 7")
        XCTAssertFalse(FileManager.default.fileExists(atPath: staging.path))
        XCTAssertNotEqual(lstat(path, &info), 0)
    }

    // MARK: Requests

    func testARequestWithoutApprovalRunsTheAdapter() throws {
        var replies: [String] = []
        let replied = expectation(description: "reply")
        listener.onReply = { replies.append($0); replied.fulfill() }
        let reply = try exchange(frame("photos.search", Self.searchInput, requestID: "r1"))
        XCTAssertEqual(status(reply), "ok")
        XCTAssertEqual(reply?["request_id"]?.stringValue, "r1")
        XCTAssertEqual(reply?["nonce"]?.stringValue, "n-r1")
        XCTAssertEqual(reply?["version"]?.integerValue, 1)
        XCTAssertEqual(reply?["result"]?["done"]?.boolValue, true)
        XCTAssertEqual(adapter.performs, 1)
        wait(for: [replied], timeout: 5)
        XCTAssertEqual(replies, ["ok"])
    }

    func testShortcutAndMediaShareOneEndpointAndApprovalLedger() throws {
        let shortcutInput: BridgeJSON = .object([
            "name": .string("LegHost bridge fixture"),
            "input": .object(["format": .string("json")]),
        ])
        let approval = try exchange(frame("shortcuts.invoke", shortcutInput,
                                          requestID: "shortcut-preview"))
        XCTAssertEqual(status(approval), "approval_required")
        let token = try XCTUnwrap(approval?["result"]?["approval_token"]?.stringValue)
        XCTAssertEqual(adapter.previews, 1)
        XCTAssertEqual(adapter.performs, 0)

        // A media request and the Shortcut continuation use the same listener
        // socket. The listener's one ledger must keep the Shortcut token while
        // routing the non-approval media capability immediately.
        let media = try exchange(frame("photos.search", Self.searchInput,
                                       requestID: "media-search"))
        XCTAssertEqual(status(media), "ok")
        let result = try exchange(frame("shortcuts.invoke", shortcutInput, token: token,
                                        requestID: "shortcut-result"))
        XCTAssertEqual(status(result), "ok")
        XCTAssertEqual(adapter.performs, 2)
        XCTAssertEqual(listener.approvals.records[BridgeApprovalLedger.key(token)]?.state,
                       .redeemed)
    }

    func testAManagedCopyIsGivenToTheGuestByItsGuestPath() throws {
        adapter.publishedCopy = "c1"
        let reply = try exchange(frame("photos.search", Self.searchInput, requestID: "r1"))
        XCTAssertEqual(status(reply), "ok")
        XCTAssertEqual(adoptions.paths, ["/root/.local/share/leg/bridge/c1"])

        adoptions.succeeds = false
        let refused = try exchange(frame("photos.search", Self.searchInput, requestID: "r2"))
        XCTAssertEqual(status(refused), "error")
        XCTAssertEqual(reason(refused), "io_failure")
    }

    func testAnUnknownCapabilityIsUnsupported() throws {
        let reply = try exchange(frame("contacts.read", .object([:])))
        XCTAssertEqual(status(reply), "unavailable")
        XCTAssertEqual(reason(reply), "capability_unsupported")
        XCTAssertEqual(adapter.performs + adapter.previews, 0)
    }

    func testABadInputIsRefusedBeforeTheAdapter() throws {
        let unknown = try exchange(frame("photos.export", .object([
            "asset_id": .string("A1"), "destination": .string("guest_workspace"), "extra": .null,
        ])))
        XCTAssertEqual(reason(unknown), "unknown_field")
        let missing = try exchange(frame("photos.export", .object(["asset_id": .string("A1")])))
        XCTAssertEqual(reason(missing), "missing_field")
        let path = try exchange(frame("photos.export", .object([
            "asset_id": .string("A1"), "destination": .string("guest_workspace"), "filename": .string("../x"),
        ])))
        XCTAssertEqual(reason(path), "invalid_path")
        XCTAssertEqual(adapter.performs + adapter.previews, 0)
    }

    func testProtocolViolationsAreRefusedWithoutNativeWork() throws {
        let input = Self.searchInput
        let digest = BridgeCanonicalJSON.digest(input)
        let encodedInput = String(decoding: BridgeCanonicalJSON.encode(input), as: UTF8.self)
        let lines = [
            // A digest that is not the digest of the input.
            frame("photos.search", input, digest: String(repeating: "0", count: 64)),
            // A deadline out of range.
            frame("photos.search", input, timeoutMs: 50),
            // Version 2.
            Data(#"{"version":2,"request_id":"r","nonce":"n","capability":"photos.search","input":\#(encodedInput),"input_sha256":"\#(digest)","timeout_ms":5000}"#.utf8),
            // An unknown field.
            Data(#"{"version":1,"request_id":"r","nonce":"n","capability":"photos.search","input":\#(encodedInput),"input_sha256":"\#(digest)","timeout_ms":5000,"x":1}"#.utf8),
            // A duplicate name in the input.
            Data(#"{"version":1,"request_id":"r","nonce":"n","capability":"photos.search","input":{"sort":"duration_desc","sort":"created_desc","filter":{"media_type":"video"}},"input_sha256":"\#(digest)","timeout_ms":5000}"#.utf8),
            // A number that is not I-JSON.
            Data(#"{"version":1,"request_id":"r","nonce":"n","capability":"photos.search","input":{"limit":1e400},"input_sha256":"\#(digest)","timeout_ms":5000}"#.utf8),
        ]
        for line in lines {
            let reply = try exchange(line)
            XCTAssertEqual(status(reply), "error", String(decoding: line, as: UTF8.self))
            XCTAssertEqual(reason(reply), "host_protocol_violation", String(decoding: line, as: UTF8.self))
        }
        XCTAssertEqual(adapter.performs + adapter.previews, 0)
    }

    func testAFrameWithoutIdentityIsClosedWithoutAReply() throws {
        XCTAssertNil(try exchange(Data("not json".utf8)))
        XCTAssertNil(try exchange(Data(#"{"version":1,"capability":"photos.search"}"#.utf8)))
        XCTAssertNil(try exchange(Data(#"{"version":1,"request_id":"r\u0001","nonce":"n"}"#.utf8)))
        var oversized = Data(#"{"version":1,"request_id":"r","nonce":"n","input":""#.utf8)
        oversized.append(Data(repeating: 0x61, count: BridgeLimits.maxFrameBytes))
        oversized.append(Data(#""}"#.utf8))
        XCTAssertNil(try exchange(oversized))
        XCTAssertEqual(adapter.performs + adapter.previews, 0)
    }

    // MARK: Approvals

    func testAPreviewIssuesATokenAndDoesNoWork() throws {
        let token = try preview()
        XCTAssertEqual(adapter.previews, 1)
        XCTAssertEqual(adapter.performs, 0)
        let record = try XCTUnwrap(listener.approvals.records[BridgeApprovalLedger.key(token)])
        XCTAssertEqual(record.capability, "photos.export")
        XCTAssertEqual(record.state, .unused)
        XCTAssertEqual(record.expiresAt, record.issuedAt + 600)
        XCTAssertFalse(listener.approvals.records.keys.contains(token), "the ledger keeps the hash of a token only")
    }

    func testAContinuationRunsOnceWithReorderedInput() throws {
        let token = try preview()
        let reply = try exchange(frame("photos.export", Self.reorderedExportInput, token: token))
        XCTAssertEqual(status(reply), "ok")
        XCTAssertEqual(adapter.performs, 1)
        XCTAssertEqual(listener.approvals.records[BridgeApprovalLedger.key(token)]?.state, .redeemed)

        let reuse = try exchange(frame("photos.export", Self.exportInput, token: token))
        XCTAssertEqual(status(reuse), "denied")
        XCTAssertEqual(reason(reuse), "approval_used")
        XCTAssertEqual(adapter.performs, 1)
    }

    func testForgedAndMismatchedTokensDoNoWork() throws {
        let forged = try exchange(frame("photos.export", Self.exportInput, token: String(repeating: "a", count: 64)))
        XCTAssertEqual(reason(forged), "approval_unknown")

        let token = try preview()
        let changed = try exchange(frame("photos.export", .object([
            "asset_id": .string("A2"), "destination": .string("guest_workspace"),
        ]), token: token))
        XCTAssertEqual(status(changed), "denied")
        XCTAssertEqual(reason(changed), "approval_mismatch")
        let otherCapability = try exchange(frame("files.export", .object([
            "copy_id": .string(String(repeating: "b", count: 32)),
            "relative_path": .string(String(repeating: "b", count: 32) + "/a.txt"),
            "destination": .string("files_picker"),
        ]), token: token))
        XCTAssertEqual(reason(otherCapability), "approval_mismatch")
        XCTAssertEqual(adapter.performs, 0)
    }

    func testATokenExpiresOneSecondAfterItsBoundary() throws {
        var clock: Int64 = 1_000_000
        listener.now = { clock }
        let atBoundary = try preview()
        let late = try preview(.object(["asset_id": .string("A9"), "destination": .string("guest_workspace")]))

        clock += 600
        XCTAssertEqual(status(try exchange(frame("photos.export", Self.exportInput, token: atBoundary))), "ok")
        clock += 1
        let expired = try exchange(frame("photos.export", .object([
            "asset_id": .string("A9"), "destination": .string("guest_workspace"),
        ]), token: late))
        XCTAssertEqual(reason(expired), "approval_expired")
        XCTAssertEqual(adapter.performs, 1)
        XCTAssertTrue(listener.approvals.records.isEmpty, "expired records are pruned")
    }

    func testADenialRedeemsTheTokenAndAFailureInvalidatesIt() throws {
        adapter.performResult = { throw BridgeFailure.denied("duplicate_destination", "taken") }
        let denied = try preview()
        XCTAssertEqual(reason(try exchange(frame("photos.export", Self.exportInput, token: denied))), "duplicate_destination")
        XCTAssertEqual(listener.approvals.records[BridgeApprovalLedger.key(denied)]?.state, .redeemed)

        adapter.performResult = { throw BridgeFailure.error("io_failure", "disk") }
        let failed = try preview()
        XCTAssertEqual(reason(try exchange(frame("photos.export", Self.exportInput, token: failed))), "io_failure")
        XCTAssertNil(listener.approvals.records[BridgeApprovalLedger.key(failed)])
        XCTAssertEqual(reason(try exchange(frame("photos.export", Self.exportInput, token: failed))), "approval_unknown")
    }

    func testTheDeadlineCancelsTheWorkAndInvalidatesTheToken() throws {
        let token = try preview()
        adapter.performResult = Self.forever
        let started = Date()
        let reply = try exchange(frame("photos.export", Self.exportInput, token: token, timeoutMs: 300))
        XCTAssertEqual(status(reply), "error")
        XCTAssertEqual(reason(reply), "timeout")
        XCTAssertLessThan(Date().timeIntervalSince(started), 5)
        XCTAssertEqual(adapter.cancelled, 1)
        XCTAssertNil(listener.approvals.records[BridgeApprovalLedger.key(token)])
    }

    func testAGuestThatHangsUpCancelsTheWork() throws {
        let token = try preview()
        adapter.performResult = Self.forever
        XCTAssertNil(try exchange(frame("photos.export", Self.exportInput, token: token), closeAfter: 0.3))
        let deadline = Date().addingTimeInterval(5)
        while adapter.cancelled == 0 && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        XCTAssertEqual(adapter.cancelled, 1)
        while listener.approvals.records[BridgeApprovalLedger.key(token)] != nil && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        XCTAssertNil(listener.approvals.records[BridgeApprovalLedger.key(token)])
    }

    func testTeardownCancelsRunningWorkAndDiscardsTheLedger() throws {
        _ = try preview()
        adapter.performResult = Self.forever
        let replied = expectation(description: "reply")
        var reply: BridgeJSON?
        DispatchQueue.global().async {
            reply = try? self.exchange(self.frame("photos.search", Self.searchInput))
            replied.fulfill()
        }
        let deadline = Date().addingTimeInterval(5)
        while adapter.performs == 0 && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        listener.beginShutdown()
        wait(for: [replied], timeout: 10)
        XCTAssertEqual(adapter.cancelled, 1)
        if let reply {
            XCTAssertEqual(status(reply), "cancelled")
        }
        listener.finishShutdown()
        XCTAssertTrue(listener.approvals.records.isEmpty)
        XCTAssertEqual(registrar.events.last, "unregister 7")
    }

    func testAnotherGenerationDoesNotHonourTheToken() throws {
        let token = try preview()
        let other = makeListener(generation: 9)
        try other.start()
        defer {
            other.beginShutdown()
            other.finishShutdown()
        }
        XCTAssertNotEqual(other.socketPath, listener.socketPath)
        let reply = try exchange(frame("photos.export", Self.exportInput, token: token), on: other)
        XCTAssertEqual(reason(reply), "approval_unknown")
        XCTAssertEqual(adapter.performs, 0)
    }

    // MARK: The ledger

    func testTheLedgerBoundaryIsInclusive() throws {
        var ledger = BridgeApprovalLedger()
        let token = ledger.issue(capability: "photos.export", inputDigest: "d", now: 100)
        let key = try ledger.begin(token: token, capability: "photos.export", inputDigest: "d", now: 700)
        XCTAssertEqual(ledger.records[key]?.state, .inFlight)
        XCTAssertThrowsError(try ledger.begin(token: token, capability: "photos.export", inputDigest: "d", now: 700)) {
            XCTAssertEqual(($0 as? BridgeFailure)?.reason, "approval_used")
        }
        ledger.finish(key: key, redeemed: true, now: 700)
        XCTAssertEqual(ledger.records[key]?.state, .redeemed)
        ledger.prune(now: 701)
        XCTAssertTrue(ledger.records.isEmpty)

        let late = ledger.issue(capability: "photos.export", inputDigest: "d", now: 100)
        XCTAssertThrowsError(try ledger.begin(token: late, capability: "photos.export", inputDigest: "d", now: 701)) {
            XCTAssertEqual(($0 as? BridgeFailure)?.reason, "approval_expired")
        }
        XCTAssertTrue(ledger.records.isEmpty)
    }

    func testTheLedgerDropsTheOldestBeyondItsCapacity() {
        var ledger = BridgeApprovalLedger()
        let first = ledger.issue(capability: "photos.export", inputDigest: "d", now: 1)
        for second in 2...Int64(BridgeApprovalLedger.capacity + 1) {
            _ = ledger.issue(capability: "photos.export", inputDigest: "d", now: second)
        }
        XCTAssertEqual(ledger.records.count, BridgeApprovalLedger.capacity)
        XCTAssertNil(ledger.records[BridgeApprovalLedger.key(first)])
        ledger.discardAll()
        XCTAssertTrue(ledger.records.isEmpty)
    }

    // MARK: The iSH hook

    func testTheHookOwnsOneGenerationAtATime() throws {
        var state = ish_bridge_socket_state()
        ish_bridge_socket_get_state(&state)
        try XCTSkipIf(state.registered != 0, "the app host has a bridge registered")

        XCTAssertEqual(ish_bridge_socket_register(41, String(repeating: "x", count: 104)), Int32(ISH_BRIDGE_SOCKET_PATH_TOO_LONG))
        XCTAssertEqual(ish_bridge_socket_register(41, "/tmp/lb-test/s"), Int32(ISH_BRIDGE_SOCKET_OK))
        defer { _ = ish_bridge_socket_unregister(41) }
        XCTAssertEqual(ish_bridge_socket_register(42, "/tmp/lb-other/s"), Int32(ISH_BRIDGE_SOCKET_BUSY))
        XCTAssertEqual(ish_bridge_socket_unregister(42), Int32(ISH_BRIDGE_SOCKET_STALE))
        ish_bridge_socket_get_state(&state)
        XCTAssertEqual(state.generation, 41)
        XCTAssertEqual(state.registered, 1)
        XCTAssertEqual(state.provisioned, 0)

        // Nothing is provisioned, so no socket maps onto the listener.
        var path = [CChar](repeating: 0, count: 104)
        XCTAssertEqual(ish_bridge_socket_connect_path(1, &path, path.count), 0)
        XCTAssertEqual(ish_bridge_socket_provision(), Int32(ISH_BRIDGE_SOCKET_NO_TASK), "provisioning needs a task")

        XCTAssertEqual(ish_bridge_socket_unregister(41), Int32(ISH_BRIDGE_SOCKET_OK))
        ish_bridge_socket_get_state(&state)
        XCTAssertEqual(state.registered, 0)
    }
}
