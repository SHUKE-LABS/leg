import Foundation
import Security

// MARK: - The contract the adapters serve

/// The status a host reply carries.
enum BridgeReplyStatus: String {
    case ok
    case approvalRequired = "approval_required"
    case denied
    case cancelled
    case unavailable
    case error
}

/// A request the host does not complete, with the status and the stable
/// reason it answers with.
struct BridgeFailure: Error, Equatable {
    let status: BridgeReplyStatus
    let reason: String
    let message: String

    static func denied(_ reason: String, _ message: String) -> BridgeFailure {
        BridgeFailure(status: .denied, reason: reason, message: message)
    }

    static func unavailable(_ reason: String, _ message: String) -> BridgeFailure {
        BridgeFailure(status: .unavailable, reason: reason, message: message)
    }

    static func error(_ reason: String, _ message: String) -> BridgeFailure {
        BridgeFailure(status: .error, reason: reason, message: message)
    }

    static let cancelled = BridgeFailure(status: .cancelled, reason: "cancelled", message: "the request was cancelled")

    static let timeout = BridgeFailure(status: .error, reason: "timeout", message: "the host ran past the request deadline")

    static func protocolViolation(_ message: String) -> BridgeFailure {
        .error("host_protocol_violation", message)
    }

    static func invalidInput(_ message: String) -> BridgeFailure {
        .error("invalid_input", message)
    }
}

/// The access the user granted to the photo library.
enum BridgePhotoAuthorization: String {
    case full
    case limited
}

/// The order `photos.search` sorts its matches in.
enum BridgePhotoSort: String {
    case durationDesc = "duration_desc"
    case createdDesc = "created_desc"
}

/// Where `photos.export` puts the copy.
enum BridgePhotoDestination: String {
    case guestWorkspace = "guest_workspace"
    case filesPicker = "files_picker"
}

/// The input of `photos.search`.
struct PhotosSearchInput: Equatable {
    var createdAfter: Date?
    var createdBefore: Date?
    var sort: BridgePhotoSort
    var limit: Int
}

/// The input of `photos.export`.
struct PhotosExportInput: Equatable {
    var assetID: String
    var destination: BridgePhotoDestination
    var filename: String?
}

/// The input of `files.export`: `relative_path` is `copy_id/name`, split.
struct FilesExportInput: Equatable {
    var copyID: String
    var name: String
    var filename: String?
}

/// A request the host serves, after its input was checked against the
/// capability's schema.
enum BridgeCapabilityRequest: Equatable {
    case photosSearch(PhotosSearchInput)
    case photosExport(PhotosExportInput)
    /// `files.import`; the only destination is the guest workspace.
    case filesImport
    case filesExport(FilesExportInput)

    /// The capabilities the host serves.
    static let capabilities = ["photos.search", "photos.export", "files.import", "files.export"]

    /// The capabilities that run only after a human approved them.
    static let approvalCapabilities: Set<String> = ["photos.export", "files.export"]

    var capability: String {
        switch self {
        case .photosSearch: return "photos.search"
        case .photosExport: return "photos.export"
        case .filesImport: return "files.import"
        case .filesExport: return "files.export"
        }
    }

    var requiresApproval: Bool { Self.approvalCapabilities.contains(capability) }

    /// Checks `input` against the schema of `capability`.
    ///
    /// A field the schema does not name is refused, at any depth: the host
    /// never guesses what an unknown field meant.
    static func parse(capability: String, input: BridgeJSON) throws -> BridgeCapabilityRequest {
        switch capability {
        case "photos.search":
            let fields = try members(input, allowed: ["filter", "sort", "limit"], required: ["filter", "sort"])
            let filter = try members(fields["filter"]!, allowed: ["media_type", "created_after", "created_before"],
                                     required: ["media_type"], name: "filter")
            guard filter["media_type"]?.stringValue == "video" else {
                throw BridgeFailure.invalidInput("filter.media_type must be \"video\"")
            }
            let after = try filter["created_after"].map { try date($0, name: "filter.created_after") }
            let before = try filter["created_before"].map { try date($0, name: "filter.created_before") }
            if let after, let before, after > before {
                throw BridgeFailure.invalidInput("filter.created_after is later than filter.created_before")
            }
            guard let sortText = fields["sort"]!.stringValue, let sort = BridgePhotoSort(rawValue: sortText) else {
                throw BridgeFailure.invalidInput("sort must be \"duration_desc\" or \"created_desc\"")
            }
            var limit = BridgeLimits.defaultSearchLimit
            if let value = fields["limit"] {
                guard let number = value.integerValue, (1...Int64(BridgeLimits.maxSearchLimit)).contains(number) else {
                    throw BridgeFailure.invalidInput("limit must be an integer from 1 to \(BridgeLimits.maxSearchLimit)")
                }
                limit = Int(number)
            }
            return .photosSearch(PhotosSearchInput(createdAfter: after, createdBefore: before, sort: sort, limit: limit))
        case "photos.export":
            let fields = try members(input, allowed: ["asset_id", "destination", "filename"],
                                     required: ["asset_id", "destination"])
            guard let assetID = fields["asset_id"]!.stringValue,
                  BridgeValidation.isPrintableID(assetID, maxBytes: BridgeLimits.maxAssetIDBytes) else {
                throw BridgeFailure.invalidInput("asset_id must be printable text of 1 to \(BridgeLimits.maxAssetIDBytes) bytes")
            }
            guard let text = fields["destination"]!.stringValue,
                  let destination = BridgePhotoDestination(rawValue: text) else {
                throw BridgeFailure.invalidInput("destination must be \"guest_workspace\" or \"files_picker\"")
            }
            let filename = try fields["filename"].map { try basename($0, name: "filename") }
            return .photosExport(PhotosExportInput(assetID: assetID, destination: destination, filename: filename))
        case "files.import":
            let fields = try members(input, allowed: ["destination"], required: ["destination"])
            guard fields["destination"]!.stringValue == "guest_workspace" else {
                throw BridgeFailure.invalidInput("destination must be \"guest_workspace\"")
            }
            return .filesImport
        case "files.export":
            let fields = try members(input, allowed: ["copy_id", "relative_path", "destination", "filename"],
                                     required: ["copy_id", "relative_path", "destination"])
            guard let copyID = fields["copy_id"]!.stringValue, BridgeValidation.isGeneratedID(copyID) else {
                throw BridgeFailure.invalidInput("copy_id is not an id the host issued")
            }
            guard let path = fields["relative_path"]!.stringValue else {
                throw BridgeFailure.invalidInput("relative_path must be a string")
            }
            guard let name = BridgeValidation.managedName(relativePath: path, copyID: copyID) else {
                throw BridgeFailure.invalidInput("relative_path must be copy_id/name")
            }
            guard fields["destination"]!.stringValue == "files_picker" else {
                throw BridgeFailure.invalidInput("destination must be \"files_picker\"")
            }
            let filename = try fields["filename"].map { try basename($0, name: "filename") }
            return .filesExport(FilesExportInput(copyID: copyID, name: name, filename: filename))
        default:
            throw BridgeFailure.unavailable("capability_unsupported", "the host does not serve \(capability)")
        }
    }

    private static func members(_ value: BridgeJSON, allowed: Set<String>, required: [String],
                                name: String = "input") throws -> [String: BridgeJSON] {
        guard case .object(let list) = value else {
            throw BridgeFailure.invalidInput("\(name) must be an object")
        }
        var fields: [String: BridgeJSON] = [:]
        for member in list {
            guard allowed.contains(member.key) else {
                throw BridgeFailure.error("unknown_field", "\(name) carries an unknown field")
            }
            fields[member.key] = member.value
        }
        for key in required where fields[key] == nil {
            throw BridgeFailure.error("missing_field", "\(name) needs \(key)")
        }
        return fields
    }

    private static func date(_ value: BridgeJSON, name: String) throws -> Date {
        guard let text = value.stringValue, let date = BridgeValidation.parseRFC3339(text) else {
            throw BridgeFailure.invalidInput("\(name) must be an RFC 3339 date-time")
        }
        return date
    }

    private static func basename(_ value: BridgeJSON, name: String) throws -> String {
        guard let text = value.stringValue, BridgeValidation.isSafeBasename(text) else {
            throw BridgeFailure.error("invalid_path", "\(name) is not a safe file name")
        }
        return text
    }
}

/// The bounds of the capability schemas, shared with the guest bridge.
enum BridgeLimits {
    static let maxInputBytes = 32 * 1024
    static let maxResultBytes = 64 * 1024
    static let maxReviewBytes = 8 * 1024
    static let maxFrameBytes = 64 * 1024
    static let maxTransferBytes: Int64 = 268_435_456
    static let defaultSearchLimit = 20
    static let maxSearchLimit = 100
    static let maxAssetIDBytes = 128
    static let maxGeneratedIDBytes = 64
    static let maxDateBytes = 64
    static let maxContentTypeBytes = 128
    static let maxDimension = 32_768
    static let maxDurationMs: Int64 = 604_800_000
    static let maxBasenameBytes = 128
    static let maxRelativePathBytes = 256
    static let defaultTimeoutMs = 5_000
    static let minTimeoutMs = 100
    static let maxTimeoutMs = 30_000
    /// How long an approval stays valid, in seconds.
    static let approvalLifetimeSeconds: Int64 = 600
}

/// The checks the capability schemas are made of.
enum BridgeValidation {
    /// Whether `text` is 1 to `maxBytes` bytes of UTF-8 without a control
    /// character.
    static func isPrintableID(_ text: String, maxBytes: Int) -> Bool {
        let count = text.utf8.count
        guard count >= 1, count <= maxBytes else { return false }
        return !text.unicodeScalars.contains { $0.properties.generalCategory == .control }
    }

    /// Whether `text` has the shape of an id the host generates:
    /// `[a-z0-9][a-z0-9-]{0,63}`.
    static func isGeneratedID(_ text: String) -> Bool {
        let bytes = Array(text.utf8)
        guard (1...BridgeLimits.maxGeneratedIDBytes).contains(bytes.count) else { return false }
        func isAlnum(_ byte: UInt8) -> Bool {
            (UInt8(ascii: "a")...UInt8(ascii: "z")).contains(byte) || (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(byte)
        }
        return isAlnum(bytes[0]) && bytes.dropFirst().allSatisfy { isAlnum($0) || $0 == UInt8(ascii: "-") }
    }

    /// A fresh id of the generated shape: 32 lowercase hex digits.
    static func newGeneratedID() -> String {
        var bytes = [UInt8](repeating: 0, count: 16)
        if SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) != errSecSuccess {
            bytes = (0..<16).map { _ in UInt8.random(in: .min ... .max) }
        }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }

    /// Whether `name` is a file name the host writes as is.
    ///
    /// A name that would need a change to be safe is refused, not changed:
    /// it must be NFC already, 1 to 128 bytes, without a separator, a colon,
    /// a control character, a leading or trailing space or dot, and not `.`
    /// or `..`.
    static func isSafeBasename(_ name: String) -> Bool {
        let bytes = Array(name.utf8)
        guard (1...BridgeLimits.maxBasenameBytes).contains(bytes.count) else { return false }
        // `String ==` compares canonical equivalence, so the bytes are compared.
        guard Array(name.precomposedStringWithCanonicalMapping.utf8) == bytes else { return false }
        guard name != ".", name != ".." else { return false }
        for scalar in name.unicodeScalars {
            switch scalar.value {
            case 0x2F, 0x5C, 0x3A, 0x00...0x1F, 0x7F...0x9F:
                return false
            default:
                continue
            }
        }
        let edges: Set<UInt8> = [UInt8(ascii: " "), UInt8(ascii: ".")]
        return !edges.contains(bytes.first!) && !edges.contains(bytes.last!)
    }

    /// The name in `relativePath`, when the path is exactly `copyID/name`
    /// with a safe name and fits the relative path bound.
    static func managedName(relativePath: String, copyID: String) -> String? {
        guard relativePath.utf8.count <= BridgeLimits.maxRelativePathBytes else { return nil }
        let prefix = copyID + "/"
        guard relativePath.hasPrefix(prefix), relativePath.utf8.starts(with: prefix.utf8) else { return nil }
        let name = String(decoding: relativePath.utf8.dropFirst(prefix.utf8.count), as: UTF8.self)
        return isSafeBasename(name) ? name : nil
    }

    /// Whether `text` is a lowercase MIME `type/subtype` without parameters.
    static func isContentType(_ text: String) -> Bool {
        let bytes = Array(text.utf8)
        guard (3...BridgeLimits.maxContentTypeBytes).contains(bytes.count) else { return false }
        let parts = bytes.split(separator: UInt8(ascii: "/"), omittingEmptySubsequences: false)
        guard parts.count == 2 else { return false }
        func isToken(_ part: ArraySlice<UInt8>) -> Bool {
            guard let first = part.first, (UInt8(ascii: "a")...UInt8(ascii: "z")).contains(first)
                    || (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(first) else { return false }
            return part.allSatisfy { byte in
                (UInt8(ascii: "a")...UInt8(ascii: "z")).contains(byte)
                    || (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(byte)
                    || "!#$&-^_.+".utf8.contains(byte)
            }
        }
        return isToken(parts[0]) && isToken(parts[1])
    }

    /// Whether `text` is a SHA-256 digest: 64 lowercase hex digits.
    static func isChecksum(_ text: String) -> Bool {
        let bytes = Array(text.utf8)
        return bytes.count == 64 && bytes.allSatisfy {
            (UInt8(ascii: "0")...UInt8(ascii: "9")).contains($0) || (UInt8(ascii: "a")...UInt8(ascii: "f")).contains($0)
        }
    }

    /// Reads a strict RFC 3339 date-time of 1 to 64 bytes:
    /// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
    static func parseRFC3339(_ text: String) -> Date? {
        let bytes = Array(text.utf8)
        guard (1...BridgeLimits.maxDateBytes).contains(bytes.count), bytes.count >= 20 else { return nil }
        var index = 0
        func number(_ width: Int) -> Int? {
            guard index + width <= bytes.count else { return nil }
            var value = 0
            for byte in bytes[index..<index + width] {
                guard (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(byte) else { return nil }
                value = value * 10 + Int(byte - UInt8(ascii: "0"))
            }
            index += width
            return value
        }
        func expect(_ characters: String) -> Bool {
            guard index < bytes.count, characters.utf8.contains(bytes[index]) else { return false }
            index += 1
            return true
        }
        guard let year = number(4), expect("-"), let month = number(2), expect("-"), let day = number(2),
              expect("Tt"), let hour = number(2), expect(":"), let minute = number(2), expect(":"),
              let second = number(2) else { return nil }
        var nanos = 0
        if index < bytes.count, bytes[index] == UInt8(ascii: ".") {
            index += 1
            var digits = 0
            while index < bytes.count, (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(bytes[index]) {
                if digits < 9 { nanos = nanos * 10 + Int(bytes[index] - UInt8(ascii: "0")) }
                digits += 1
                index += 1
            }
            guard (1...9).contains(digits) else { return nil }
            for _ in digits..<9 { nanos *= 10 }
        }
        var offset = 0
        guard index < bytes.count else { return nil }
        if bytes[index] == UInt8(ascii: "Z") || bytes[index] == UInt8(ascii: "z") {
            index += 1
        } else {
            let sign = bytes[index] == UInt8(ascii: "+") ? 1 : (bytes[index] == UInt8(ascii: "-") ? -1 : 0)
            guard sign != 0 else { return nil }
            index += 1
            guard let offsetHours = number(2), expect(":"), let offsetMinutes = number(2),
                  offsetHours <= 23, offsetMinutes <= 59 else { return nil }
            offset = sign * (offsetHours * 3600 + offsetMinutes * 60)
        }
        guard index == bytes.count else { return nil }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
        let monthDays = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        guard (1...12).contains(month), (1...monthDays[month - 1]).contains(day),
              hour <= 23, minute <= 59, second <= 59 else { return nil }
        let seconds = daysFromCivil(year: year, month: month, day: day) * 86_400
            + hour * 3600 + minute * 60 + second - offset
        return Date(timeIntervalSince1970: TimeInterval(seconds) + TimeInterval(nanos) / 1_000_000_000)
    }

    /// The RFC 3339 text of `date` in UTC, to the second.
    static func formatRFC3339(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        formatter.timeZone = TimeZone(secondsFromGMT: 0)
        return formatter.string(from: date)
    }

    /// The days from 1970-01-01 to a proleptic Gregorian date.
    private static func daysFromCivil(year: Int, month: Int, day: Int) -> Int {
        let y = month <= 2 ? year - 1 : year
        let era = (y >= 0 ? y : y - 399) / 400
        let yearOfEra = y - era * 400
        let dayOfYear = (153 * (month + (month > 2 ? -3 : 9)) + 2) / 5 + day - 1
        let dayOfEra = yearOfEra * 365 + yearOfEra / 4 - yearOfEra / 100 + dayOfYear
        return era * 146_097 + dayOfEra - 719_468
    }
}

/// Where one operation may write.
struct BridgeOperationPaths: Equatable {
    /// The operation's own staging directory:
    /// `Library/Caches/LegHost/BridgeStaging/<generation>/<operation-id>/`.
    /// The adapter creates it, and removes it when the operation fails.
    let staging: URL
    /// The managed copies in the guest home:
    /// `<active root>/data/root/.local/share/leg/bridge`, which the guest
    /// reads as `~/.local/share/leg/bridge`.
    let managedRoot: URL
}

/// What an adapter knows about the operation it runs.
struct BridgeOperationContext {
    /// The guest generation the request came from.
    let generation: String
    /// The id of this operation, of the generated shape.
    let operationID: String
    /// When the request runs out of time.
    let deadline: Date
    let paths: BridgeOperationPaths
    /// Gives the guest the managed copy `<managed root>/<copy_id>` the
    /// operation committed: the guest sees a file the host wrote only once
    /// its fakefs has an entry for it. A throw fails the operation.
    var publishManagedCopy: (_ copyID: String) throws -> Void = { _ in }
}

/// A host adapter that serves capabilities.
///
/// The listener checked the envelope, the schema, and the approval before
/// it calls an adapter. Cancellation is the Swift task's: the listener
/// cancels the task at the deadline and at teardown, and an adapter that
/// sees it stops, removes what it staged, and throws `CancellationError` or
/// `BridgeFailure.cancelled`.
protocol BridgeCapabilityAdapter: AnyObject {
    /// The review summary of a capability that needs an approval.
    ///
    /// A preview has no side effect: it opens no picker, reads no content,
    /// stages nothing, and changes no destination. It may look up metadata
    /// to say what would happen.
    func preview(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON

    /// Runs the capability and returns its result.
    ///
    /// For a capability that needs an approval, the listener calls this only
    /// with an approval it verified. A failure is a thrown `BridgeFailure`.
    func perform(_ request: BridgeCapabilityRequest, context: BridgeOperationContext) async throws -> BridgeJSON
}

// MARK: - The approval ledger

/// The approvals the host issued to one guest generation.
///
/// The ledger lives in memory and belongs to one listener, so a new
/// generation starts with none. It keeps the SHA-256 of a token, never the
/// token. An approval is valid while `now <= expiresAt`, with
/// `expiresAt = issuedAt + 600` in whole Unix seconds, the rule the guest
/// bridge applies too. Expired records are pruned before every lookup,
/// insert, and transition.
struct BridgeApprovalLedger {
    enum State: Equatable {
        case unused
        /// A continuation is running on the approval.
        case inFlight
        case redeemed
    }

    struct Record: Equatable {
        let capability: String
        let inputDigest: String
        let issuedAt: Int64
        let expiresAt: Int64
        var state: State
    }

    /// The most approvals the ledger holds; the oldest goes first.
    static let capacity = 32

    private(set) var records: [String: Record] = [:]

    /// Issues a token for `capability` and `inputDigest`.
    mutating func issue(capability: String, inputDigest: String, now: Int64) -> String {
        prune(now: now)
        while records.count >= Self.capacity,
              let oldest = records.min(by: { $0.value.issuedAt < $1.value.issuedAt })?.key {
            records.removeValue(forKey: oldest)
        }
        let token = BridgeValidation.newGeneratedID() + BridgeValidation.newGeneratedID()
        records[Self.key(token)] = Record(capability: capability, inputDigest: inputDigest, issuedAt: now,
                                          expiresAt: now + BridgeLimits.approvalLifetimeSeconds, state: .unused)
        return token
    }

    /// Starts a continuation on `token`; the returned key finishes it.
    mutating func begin(token: String, capability: String, inputDigest: String, now: Int64) throws -> String {
        let key = Self.key(token)
        defer { prune(now: now) }
        guard let record = records[key] else {
            throw BridgeFailure.denied("approval_unknown", "the host did not issue the approval token")
        }
        guard now <= record.expiresAt else {
            records.removeValue(forKey: key)
            throw BridgeFailure.denied("approval_expired", "the approval expired")
        }
        guard record.state == .unused else {
            throw BridgeFailure.denied("approval_used", "the approval was already used")
        }
        guard record.capability == capability, record.inputDigest == inputDigest else {
            throw BridgeFailure.denied("approval_mismatch", "the approval was issued for another request")
        }
        records[key]?.state = .inFlight
        return key
    }

    /// Ends the continuation on `key`: a decision redeems the approval, and
    /// anything else — a cancel, a timeout, a failure — invalidates it.
    mutating func finish(key: String, redeemed: Bool, now: Int64) {
        prune(now: now)
        if redeemed {
            records[key]?.state = .redeemed
        } else {
            records.removeValue(forKey: key)
        }
    }

    mutating func prune(now: Int64) {
        records = records.filter { now <= $0.value.expiresAt }
    }

    mutating func discardAll() {
        records.removeAll()
    }

    static func key(_ token: String) -> String {
        BridgeCanonicalJSON.sha256Hex(Data(token.utf8))
    }
}

// MARK: - The guest endpoint

/// Maps the guest endpoint onto the listener.
protocol BridgeEndpointRegistrar: AnyObject {
    /// Maps the guest endpoint of `generation` onto `path`. Returns 0, or
    /// one of the `ISH_BRIDGE_SOCKET_*` failures.
    func register(generation: UInt64, path: String) -> Int32
    func unregister(generation: UInt64)
}

/// The registrar of the vendored iSH runtime.
final class IshBridgeEndpointRegistrar: BridgeEndpointRegistrar {
    func register(generation: UInt64, path: String) -> Int32 {
        path.withCString { ish_bridge_socket_register(generation, $0) }
    }

    func unregister(generation: UInt64) {
        _ = ish_bridge_socket_unregister(generation)
    }
}

// MARK: - The listener

/// Why the listener could not start.
struct BridgeListenerError: Error, LocalizedError {
    static let diagnostic = "IOS_BRIDGE_LISTENER_ERROR"

    let message: String

    var errorDescription: String? { "\(Self.diagnostic): \(message)" }
}

/// The host side of the capability bridge, for one guest generation.
///
/// The listener is a real AF_UNIX socket in a private directory of the app.
/// The guest reaches it at `/run/leghost-bridge.sock`, which the vendored
/// iSH runtime maps onto it; the guest never sees the host path. Every
/// connection carries one bounded request line and gets at most one bounded
/// reply line:
///
/// - a frame that is not a HostRequest of version 1 — malformed, oversized,
///   with an unknown field, a duplicate name, a number that is not I-JSON, or
///   an `input_sha256` that is not the digest of the input — is refused with
///   `error/host_protocol_violation` when it names a usable request id and
///   nonce, and closed without a reply otherwise;
/// - a capability that needs an approval and carries no token is a preview:
///   the adapter describes what it would do, and the host issues a token;
/// - a continuation runs only on a token this listener issued for the same
///   capability and input, unused and not expired.
///
/// The work of an adapter is cancelled at the request deadline, when the
/// guest closes the connection, and at teardown.
final class BridgeListener: @unchecked Sendable {
    /// The path the guest connects to.
    static let guestEndpoint = ISH_BRIDGE_SOCKET_GUEST_PATH

    /// The variable that tells the guest bridge where the host listens.
    static let environmentVariable = "LEG_BRIDGE_HOST_SOCKET"

    /// The managed root as the guest names it.
    static let managedGuestPath = "/root/.local/share/leg/bridge"

    /// How long the listener waits for the request line of a connection.
    static let frameTimeout: TimeInterval = 5

    /// The directory the private socket directories are made in.
    ///
    /// A Darwin socket path holds 103 bytes. The temporary directory of an
    /// app on a device fits; the one of a simulator app is far longer, so
    /// the simulator uses the shared temporary directory of the Mac.
    static var defaultSocketParent: URL {
        #if targetEnvironment(simulator)
        return URL(fileURLWithPath: "/tmp", isDirectory: true)
        #else
        return URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
        #endif
    }

    let generation: UInt64
    /// The id of this generation in the staging paths.
    let generationID: String
    private(set) var socketPath = ""

    private let adapters: [String: BridgeCapabilityAdapter]
    private let registrar: BridgeEndpointRegistrar
    private let socketParent: URL
    private let stagingRoot: URL
    private let managedRoot: URL
    private let guestAdopter: (String) -> Bool
    /// The clock of the ledger, in whole Unix seconds.
    var now: () -> Int64 = { Int64(Date().timeIntervalSince1970.rounded(.down)) }
    /// Called with the status of every reply the listener wrote.
    var onReply: ((String) -> Void)?
    /// How long the teardown waits for the workers before it leaves the rest
    /// of the teardown to the last worker that ends.
    var joinTimeout: DispatchTimeInterval = .seconds(10)
    /// How long a stopped request waits for its adapter before it answers.
    var cancelGrace: DispatchTimeInterval = .seconds(5)
    /// Called with the socket path right before the bind, for the tests.
    var beforeBind: ((String) -> Void)?

    private let lock = NSLock()
    private var ledger = BridgeApprovalLedger()
    private var listenFD: Int32 = -1
    private var wakeFDs: [Int32] = [-1, -1]
    private var socketDirectory: URL?
    /// The device and inode of the socket this listener bound: the teardown
    /// removes that socket and nothing else.
    private var boundSocket: (device: dev_t, inode: ino_t)?
    private var registered = false
    private var accepting = false
    private var connections: Set<Int32> = []
    private var tasks: [Int: Task<Void, Never>] = [:]
    private var nextTask = 0
    private let workers = DispatchGroup()

    init(generation: UInt64,
         adapters: [String: BridgeCapabilityAdapter],
         registrar: BridgeEndpointRegistrar,
         socketParent: URL = BridgeListener.defaultSocketParent,
         stagingRoot: URL,
         managedRoot: URL,
         guestAdopter: @escaping (_ guestPath: String) -> Bool = { _ in true }) {
        self.generation = generation
        self.generationID = BridgeValidation.newGeneratedID()
        self.adapters = adapters
        self.registrar = registrar
        self.socketParent = socketParent
        self.stagingRoot = stagingRoot
        self.managedRoot = managedRoot
        self.guestAdopter = guestAdopter
    }

    deinit {
        // A connection and the work of an adapter hold the listener, so only
        // the accept loop can still run here: the wake of beginShutdown ends
        // it, and the wait is bounded because this may be its own thread.
        beginShutdown()
        _ = workers.wait(timeout: .now() + joinTimeout)
        release()
    }

    /// The approvals the listener holds, for the tests.
    var approvals: BridgeApprovalLedger {
        lock.lock()
        defer { lock.unlock() }
        return ledger
    }

    // MARK: Start and stop

    /// Binds and listens on a fresh private socket and maps the guest
    /// endpoint onto it.
    func start() throws {
        let directory = socketParent.appendingPathComponent(
            "lb-" + String(BridgeValidation.newGeneratedID().prefix(12)), isDirectory: true)
        guard mkdir(directory.path, 0o700) == 0 else {
            throw BridgeListenerError(message: "the socket directory could not be made (errno \(errno))")
        }
        socketDirectory = directory
        let path = directory.appendingPathComponent("s").path
        socketPath = path
        do {
            try bindAndListen(path: path)
            let status = registrar.register(generation: generation, path: path)
            guard status == 0 else {
                throw BridgeListenerError(message: "the guest endpoint could not be mapped (\(status))")
            }
            registered = true
            try startAccepting()
        } catch {
            beginShutdown()
            finishShutdown()
            throw error
        }
    }

    private func bindAndListen(path: String) throws {
        var address = sockaddr_un()
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        let bytes = Array(path.utf8)
        guard bytes.count < capacity, bytes.count <= Int(ISH_BRIDGE_SOCKET_HOST_PATH_MAX) else {
            throw BridgeListenerError(message: "the socket path is longer than a Darwin socket path")
        }
        var info = stat()
        guard lstat(path, &info) != 0, errno == ENOENT else {
            throw BridgeListenerError(message: "the socket path is already taken")
        }
        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw BridgeListenerError(message: "the socket could not be made (errno \(errno))") }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        beforeBind?(path)
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0 else {
            let code = errno
            close(fd)
            throw BridgeListenerError(message: "the socket could not be bound (errno \(code))")
        }
        // The private directory holds nothing else, so the socket at the path
        // right after the bind is the one this listener made.
        var owned = stat()
        if lstat(path, &owned) == 0, owned.st_mode & S_IFMT == S_IFSOCK {
            boundSocket = (owned.st_dev, owned.st_ino)
        }
        guard boundSocket != nil, chmod(path, 0o600) == 0, listen(fd, 8) == 0 else {
            let code = errno
            close(fd)
            throw BridgeListenerError(message: "the socket could not listen (errno \(code))")
        }
        listenFD = fd
    }

    private func startAccepting() throws {
        var fds: [Int32] = [-1, -1]
        guard pipe(&fds) == 0 else { throw BridgeListenerError(message: "the wake pipe could not be made") }
        lock.lock()
        wakeFDs = fds
        accepting = true
        lock.unlock()
        let listenFD = self.listenFD
        let wake = fds[0]
        workers.enter()
        let thread = Thread { [weak self] in
            Self.acceptLoop(listenFD: listenFD, wakeFD: wake) { fd in
                guard let self else {
                    close(fd)
                    return
                }
                self.accepted(fd)
            }
            self?.workers.leave()
        }
        thread.name = "LegHost bridge listener"
        thread.start()
    }

    private static func acceptLoop(listenFD: Int32, wakeFD: Int32, handle: (Int32) -> Void) {
        while true {
            var pollFDs = [pollfd(fd: listenFD, events: Int16(POLLIN), revents: 0),
                           pollfd(fd: wakeFD, events: Int16(POLLIN), revents: 0)]
            let ready = poll(&pollFDs, 2, -1)
            if ready < 0 {
                if errno == EINTR { continue }
                return
            }
            if pollFDs[1].revents != 0 { return }
            if pollFDs[0].revents & Int16(POLLIN) != 0 {
                let client = accept(listenFD, nil, nil)
                if client >= 0 {
                    handle(client)
                } else if errno != EINTR && errno != ECONNABORTED {
                    return
                }
            }
        }
    }

    private func accepted(_ fd: Int32) {
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        var on: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        lock.lock()
        guard accepting else {
            lock.unlock()
            close(fd)
            return
        }
        connections.insert(fd)
        workers.enter()
        lock.unlock()
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            serve(fd)
            lock.lock()
            connections.remove(fd)
            lock.unlock()
            close(fd)
            workers.leave()
        }
    }

    /// The first half of the teardown: no new work starts, the work of the
    /// adapters is cancelled, and the connections are shut down. The guest
    /// is stopped after this.
    func beginShutdown() {
        lock.lock()
        accepting = false
        let running = Array(tasks.values)
        for fd in connections {
            shutdown(fd, SHUT_RDWR)
        }
        let wake = wakeFDs[1]
        lock.unlock()
        running.forEach { $0.cancel() }
        if wake >= 0 {
            var byte: UInt8 = 1
            _ = write(wake, &byte, 1)
        }
    }

    /// The second half of the teardown, after the guest stopped: the workers
    /// are joined, the endpoint mapping is dropped, and the socket, its
    /// directory, and the staging of this generation are removed.
    ///
    /// An adapter that does not stop within `joinTimeout` may still write,
    /// so the mapping, the socket, and the staging stay until the last
    /// worker ended, and the teardown finishes then. Returns whether it
    /// finished now.
    @discardableResult
    func finishShutdown() -> Bool {
        guard workers.wait(timeout: .now() + joinTimeout) == .success else {
            NSLog("[LegHost] the bridge workers did not stop; the teardown waits for them")
            workers.notify(queue: .global(qos: .utility)) { [self] in release() }
            return false
        }
        release()
        return true
    }

    /// Drops the mapping and removes the socket, its directory, and the
    /// staging of this generation. Runs once no worker is left.
    private func release() {
        lock.lock()
        defer { lock.unlock() }
        ledger.discardAll()
        if registered {
            registrar.unregister(generation: generation)
            registered = false
        }
        if listenFD >= 0 {
            close(listenFD)
            listenFD = -1
        }
        for index in wakeFDs.indices where wakeFDs[index] >= 0 {
            close(wakeFDs[index])
            wakeFDs[index] = -1
        }
        if let directory = socketDirectory {
            // Only the socket this listener bound, and the directory it made
            // when it is empty.
            var info = stat()
            if let owned = boundSocket, lstat(socketPath, &info) == 0, info.st_mode & S_IFMT == S_IFSOCK,
               info.st_dev == owned.device, info.st_ino == owned.inode {
                unlink(socketPath)
            }
            boundSocket = nil
            rmdir(directory.path)
            socketDirectory = nil
        }
        try? FileManager.default.removeItem(at: stagingRoot.appendingPathComponent(generationID, isDirectory: true))
    }

    // MARK: One connection

    /// Reads the request line, or nothing when the connection ended, ran out
    /// of time, or sent more than a frame.
    private func readFrame(_ fd: Int32) -> Data? {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 16 * 1024)
        let deadline = Date().addingTimeInterval(Self.frameTimeout)
        while true {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { return nil }
            var pollFD = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            let ready = poll(&pollFD, 1, Int32(min(remaining, 1) * 1000) + 1)
            if ready < 0 && errno != EINTR { return nil }
            guard ready > 0 else { continue }
            let count = read(fd, &chunk, chunk.count)
            if count < 0 {
                if errno == EINTR || errno == EAGAIN { continue }
                return nil
            }
            if count == 0 { return nil }
            buffer.append(contentsOf: chunk[0..<count])
            if let newline = buffer.firstIndex(of: 0x0A) {
                let line = buffer[buffer.startIndex..<newline]
                return line.count <= BridgeLimits.maxFrameBytes ? Data(line) : nil
            }
            if buffer.count > BridgeLimits.maxFrameBytes { return nil }
        }
    }

    private func serve(_ fd: Int32) {
        guard let line = readFrame(fd), let reply = process(line, fd: fd) else { return }
        var bytes = Array(BridgeCanonicalJSON.encode(reply.body))
        bytes.append(0x0A)
        var offset = 0
        while offset < bytes.count {
            let written = bytes[offset...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            if written < 0 {
                if errno == EINTR || errno == EAGAIN { continue }
                return
            }
            offset += written
        }
        onReply?(reply.status.rawValue)
    }

    private struct Reply {
        let status: BridgeReplyStatus
        let body: BridgeJSON
    }

    private func process(_ line: Data, fd: Int32) -> Reply? {
        // A frame without a usable identity cannot be answered: the guest
        // would not take a reply that names no request of its own. A frame
        // the strict parser refuses — a duplicate name, a number that is not
        // I-JSON — still gets its refusal when a lenient read finds one.
        let strict = try? BridgeCanonicalJSON.parse(line)
        guard let envelope = strict ?? (try? BridgeCanonicalJSON.parse(line, lenient: true)),
              let requestID = envelope["request_id"]?.stringValue,
              let nonce = envelope["nonce"]?.stringValue,
              BridgeValidation.isPrintableID(requestID, maxBytes: 128),
              BridgeValidation.isPrintableID(nonce, maxBytes: 128) else {
            return nil
        }
        func reply(_ status: BridgeReplyStatus, result: BridgeJSON = .null, error: BridgeFailure? = nil) -> Reply {
            let errorValue: BridgeJSON = error.map {
                .object(["reason": .string($0.reason), "message": .string($0.message)])
            } ?? .null
            return Reply(status: status, body: .object([
                "version": .integer(1),
                "nonce": .string(nonce),
                "request_id": .string(requestID),
                "status": .string(status.rawValue),
                "result": result,
                "error": errorValue,
            ]))
        }
        func failure(_ failure: BridgeFailure) -> Reply {
            reply(failure.status, error: failure)
        }

        guard let value = strict else {
            return failure(.protocolViolation("the request is not canonical JSON"))
        }
        let request: HostRequest
        do {
            request = try HostRequest(value)
        } catch let error as BridgeFailure {
            return failure(error)
        } catch {
            return failure(.protocolViolation("the request is not a host request"))
        }

        guard let adapter = adapters[request.capability] else {
            return failure(.unavailable("capability_unsupported", "the host does not serve \(request.capability)"))
        }
        let typed: BridgeCapabilityRequest
        do {
            typed = try BridgeCapabilityRequest.parse(capability: request.capability, input: request.input)
        } catch let error as BridgeFailure {
            return failure(error)
        } catch {
            return failure(.invalidInput("the input does not fit the capability"))
        }
        let operationID = BridgeValidation.newGeneratedID()
        let context = BridgeOperationContext(
            generation: generationID,
            operationID: operationID,
            deadline: Date().addingTimeInterval(TimeInterval(request.timeoutMs) / 1000),
            paths: BridgeOperationPaths(
                staging: stagingRoot.appendingPathComponent(generationID, isDirectory: true)
                    .appendingPathComponent(operationID, isDirectory: true),
                managedRoot: managedRoot),
            publishManagedCopy: { [guestAdopter] copyID in
                guard guestAdopter("\(Self.managedGuestPath)/\(copyID)") else {
                    throw BridgeFailure.error("io_failure", "the guest could not be given the managed copy")
                }
            })

        // A preview: the adapter says what it would do, and nothing happens.
        if typed.requiresApproval && request.approvalToken == nil {
            switch run(fd: fd, deadline: context.deadline, { try await adapter.preview(typed, context: context) }) {
            case .failure(let error):
                return failure(error)
            case .success(let review):
                guard BridgeCanonicalJSON.encode(review).count <= BridgeLimits.maxReviewBytes else {
                    return failure(.error("result_too_large", "the review summary is larger than the limit"))
                }
                lock.lock()
                let token = ledger.issue(capability: request.capability, inputDigest: request.inputDigest, now: now())
                lock.unlock()
                return reply(.approvalRequired, result: .object(["approval_token": .string(token), "review": review]))
            }
        }

        var ledgerKey: String?
        if let token = request.approvalToken {
            lock.lock()
            defer { lock.unlock() }
            do {
                ledgerKey = try ledger.begin(token: token, capability: request.capability,
                                             inputDigest: request.inputDigest, now: now())
            } catch let error as BridgeFailure {
                return failure(error)
            } catch {
                return failure(.denied("approval_unknown", "the approval token is not usable"))
            }
        }

        let answer: Reply
        switch run(fd: fd, deadline: context.deadline, { try await adapter.perform(typed, context: context) }) {
        case .success(let result):
            if BridgeCanonicalJSON.encode(result).count > BridgeLimits.maxResultBytes {
                answer = failure(.error("result_too_large", "the result is larger than the limit"))
            } else {
                answer = reply(.ok, result: result)
            }
        case .failure(let error):
            answer = failure(error)
        }
        if let ledgerKey {
            let decided = answer.status == .ok || answer.status == .denied
            lock.lock()
            ledger.finish(key: ledgerKey, redeemed: decided, now: now())
            lock.unlock()
        }
        return answer
    }

    /// Runs the work of an adapter until it ends, the deadline passes, the
    /// guest closes the connection, or the listener shuts down.
    private func run(fd: Int32, deadline: Date,
                     _ body: @escaping () async throws -> BridgeJSON) -> Result<BridgeJSON, BridgeFailure> {
        let done = DispatchSemaphore(value: 0)
        let box = ResultBox()
        lock.lock()
        guard accepting else {
            lock.unlock()
            return .failure(.cancelled)
        }
        let id = nextTask
        nextTask += 1
        // The work counts as a worker, and holds the listener, until the
        // adapter returns, also when the request was answered before: the
        // teardown joins it.
        workers.enter()
        let task = Task { [self] in
            do {
                box.set(.success(try await body()))
            } catch let error as BridgeFailure {
                box.set(.failure(error))
            } catch is CancellationError {
                box.set(.failure(.cancelled))
            } catch {
                box.set(.failure(.error("io_failure", "the host failed")))
            }
            done.signal()
            workers.leave()
        }
        tasks[id] = task
        lock.unlock()
        defer {
            lock.lock()
            tasks.removeValue(forKey: id)
            lock.unlock()
        }

        while done.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
            var stopReason: BridgeFailure?
            if Date() >= deadline {
                stopReason = .timeout
            } else if Self.peerClosed(fd) || !isAccepting {
                stopReason = .cancelled
            }
            if let stopReason {
                task.cancel()
                // The adapter removes what it staged before it returns.
                _ = done.wait(timeout: .now() + cancelGrace)
                return .failure(stopReason)
            }
        }
        return box.get() ?? .failure(.error("io_failure", "the host failed"))
    }

    private var isAccepting: Bool {
        lock.lock()
        defer { lock.unlock() }
        return accepting
    }

    /// Whether the guest closed its end of the connection.
    private static func peerClosed(_ fd: Int32) -> Bool {
        var pollFD = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
        guard poll(&pollFD, 1, 0) > 0 else { return false }
        if pollFD.revents & Int16(POLLHUP | POLLERR) != 0 { return true }
        var byte: UInt8 = 0
        return recv(fd, &byte, 1, MSG_PEEK | MSG_DONTWAIT) == 0
    }

    private final class ResultBox: @unchecked Sendable {
        private let lock = NSLock()
        private var value: Result<BridgeJSON, BridgeFailure>?

        func set(_ result: Result<BridgeJSON, BridgeFailure>) {
            lock.lock()
            value = result
            lock.unlock()
        }

        func get() -> Result<BridgeJSON, BridgeFailure>? {
            lock.lock()
            defer { lock.unlock() }
            return value
        }
    }
}

/// A HostRequest of version 1, checked field by field.
struct HostRequest {
    /// The fields a HostRequest may carry.
    static let fields: Set<String> = [
        "version", "request_id", "nonce", "capability", "input", "input_sha256", "approval_token", "timeout_ms",
    ]

    let requestID: String
    let nonce: String
    let capability: String
    let input: BridgeJSON
    /// The digest the host computed; the request's own only has to agree.
    let inputDigest: String
    let approvalToken: String?
    let timeoutMs: Int

    init(_ value: BridgeJSON) throws {
        guard let keys = value.keys else { throw BridgeFailure.protocolViolation("the request is not an object") }
        guard keys.allSatisfy(Self.fields.contains) else {
            throw BridgeFailure.protocolViolation("the request carries an unknown field")
        }
        guard value["version"]?.integerValue == 1 else {
            throw BridgeFailure.protocolViolation("the request does not speak version 1")
        }
        guard let requestID = value["request_id"]?.stringValue, let nonce = value["nonce"]?.stringValue else {
            throw BridgeFailure.protocolViolation("the request has no identity")
        }
        guard let capability = value["capability"]?.stringValue,
              BridgeValidation.isPrintableID(capability, maxBytes: 64) else {
            throw BridgeFailure.protocolViolation("the request names no capability")
        }
        guard let input = value["input"], case .object = input else {
            throw BridgeFailure.protocolViolation("the request input is not an object")
        }
        guard BridgeCanonicalJSON.encode(input).count <= BridgeLimits.maxInputBytes else {
            throw BridgeFailure.protocolViolation("the request input is larger than the limit")
        }
        let computed = BridgeCanonicalJSON.digest(input)
        guard let digest = value["input_sha256"]?.stringValue, digest == computed else {
            throw BridgeFailure.protocolViolation("the input digest does not match the input")
        }
        var token: String?
        if let field = value["approval_token"] {
            guard let text = field.stringValue, BridgeValidation.isPrintableID(text, maxBytes: 256) else {
                throw BridgeFailure.protocolViolation("the approval token is not a token")
            }
            token = text
        }
        guard let timeout = value["timeout_ms"]?.integerValue,
              (Int64(BridgeLimits.minTimeoutMs)...Int64(BridgeLimits.maxTimeoutMs)).contains(timeout) else {
            throw BridgeFailure.protocolViolation("the request deadline is out of range")
        }
        self.requestID = requestID
        self.nonce = nonce
        self.capability = capability
        self.input = input
        self.inputDigest = computed
        self.approvalToken = token
        self.timeoutMs = Int(timeout)
    }
}
