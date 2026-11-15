import Foundation

#if canImport(Darwin)
import Darwin
#endif

/// What one run of the Skill installer did.
enum GuestSkillInstallOutcome: Equatable {
    /// The target was empty and the bundled package is now there.
    case installed
    /// An older managed package was replaced by the bundled one.
    case updated
    /// The managed package already is the bundled one.
    case unchanged
    /// The managed package is newer than the bundled one and was kept.
    case olderBundled
}

/// Why the Skill installer stopped.
///
/// The reason is stable, so the host and the tests can act on it:
/// `install_conflict` (the target is not a package the installer may
/// replace; it was preserved), `invalid_package` (the bundled package breaks
/// the package rules), `insufficient_space`, and `io_failure`.
struct GuestSkillInstallError: Error, LocalizedError, Equatable {
    let reason: String
    let message: String

    var errorDescription: String? { "\(reason): \(message)" }

    static func conflict(_ message: String) -> GuestSkillInstallError {
        GuestSkillInstallError(reason: "install_conflict", message: message)
    }

    static func invalidPackage(_ message: String) -> GuestSkillInstallError {
        GuestSkillInstallError(reason: "invalid_package", message: message)
    }

    static func io(_ message: String) -> GuestSkillInstallError {
        GuestSkillInstallError(reason: "io_failure", message: message)
    }
}

/// Installs the bundled `mobile-media-files` Skill into the guest home.
///
/// The package lands at `<active root>/data/root/.agents/skills/mobile-media-files/`,
/// which the guest discovers as the user Skills root `~/.agents/skills`. The
/// installed package carries `.leg-manifest.json`, the proof that the
/// installer owns it:
///
/// ```json
/// {"digest":"…","digest_algorithm":"sha256","owner":"leg-ios-bundled","package":"mobile-media-files","schema":"leg.skill-install/v1","version":"1.0.0"}
/// ```
///
/// A target the installer does not own — no manifest, a manifest that is not
/// this one, or files that no longer match the manifest digest — is a
/// conflict and is never touched. The installer stages the package beside
/// the target, under `.agents/.leg-skill-install/`, writes a recovery journal,
/// and swaps the staged package into place in one rename; `recover()` finishes
/// or undoes an install a crash interrupted, and removes only what the
/// installer itself staged.
final class GuestSkillInstaller {
    static let packageName = "mobile-media-files"
    static let manifestName = ".leg-manifest.json"
    static let manifestSchema = "leg.skill-install/v1"
    static let owner = "leg-ios-bundled"
    static let journalSchema = "leg.skill-install-journal/v1"
    /// The domain the package digest starts with, followed by one NUL byte.
    static let digestDomain = "leg.skill.package/v1"

    /// The points a test can make the install fail at, as a crash would.
    enum Fault: Hashable {
        /// After the first staged file was written.
        case copy
        /// The free space check reports too little room.
        case space
        /// The staged package does not hash to the bundled digest.
        case hash
        /// Right after the staged package was swapped into place, before the
        /// journal records it.
        case swap
        /// After the staged manifest was written, before it was made durable.
        case manifest
    }

    /// The faults the next install runs into. Tests only.
    var faults: Set<Fault> = []

    private let bundledPackage: URL
    private let skillsRoot: URL
    private let target: URL
    private let workDirectory: URL
    private let staging: URL
    private let journal: URL
    private let journalTemporary: URL

    /// - Parameters:
    ///   - bundledPackage: the `mobile-media-files` folder in the app bundle.
    ///   - activeRoot: the active guest root; the guest home is `data/root`.
    init(bundledPackage: URL, activeRoot: URL) {
        self.bundledPackage = bundledPackage
        let agents = activeRoot.appendingPathComponent("data/root/.agents", isDirectory: true)
        skillsRoot = agents.appendingPathComponent("skills", isDirectory: true)
        target = skillsRoot.appendingPathComponent(Self.packageName, isDirectory: true)
        workDirectory = agents.appendingPathComponent(".leg-skill-install", isDirectory: true)
        staging = workDirectory.appendingPathComponent("\(Self.packageName).staging", isDirectory: true)
        journal = workDirectory.appendingPathComponent("\(Self.packageName).journal")
        journalTemporary = workDirectory.appendingPathComponent("\(Self.packageName).journal.tmp")
    }

    /// The directory the package is installed in.
    var installedPackage: URL { target }

    // MARK: - Install

    /// Installs the bundled package, after finishing an interrupted install.
    @discardableResult
    func install() throws -> GuestSkillInstallOutcome {
        try recover()
        let bundled = try readBundledPackage()

        let installed: Manifest?
        switch fileType(target.path) {
        case .absent:
            installed = nil
        case .directory:
            installed = try managedManifest(of: target)
        case .regular, .other:
            throw GuestSkillInstallError.conflict("the Skill target is not a directory; it was left as it is")
        }

        if let installed {
            if installed.digest == bundled.digest { return .unchanged }
            if bundled.version < installed.version { return .olderBundled }
            if bundled.version == installed.version {
                throw GuestSkillInstallError.conflict(
                    "the installed package has version \(installed.version) with another digest; it was left as it is")
            }
        }

        try ensureDirectory(skillsRoot)
        try ensureDirectory(workDirectory)
        try checkSpace(for: bundled)
        let manifest = Manifest(version: bundled.version, digest: bundled.digest)
        try writeJournal(Journal(version: bundled.version, digest: bundled.digest, phase: .staging))
        try stage(bundled, manifest: manifest)

        let swap = installed != nil
        let flags = UInt32(swap ? RENAME_SWAP : RENAME_EXCL)
        guard renamex_np(staging.path, target.path, flags) == 0 else {
            let code = errno
            if code == EEXIST {
                throw GuestSkillInstallError.conflict("a package appeared at the Skill target; it was left as it is")
            }
            throw GuestSkillInstallError.io("the staged package could not be moved into place: \(String(cString: strerror(code)))")
        }
        try syncDirectory(skillsRoot)
        try syncDirectory(workDirectory)
        if faults.contains(.swap) {
            throw GuestSkillInstallError.io("injected failure after the swap")
        }
        try writeJournal(Journal(version: bundled.version, digest: bundled.digest, phase: .committed))
        // The staging path now holds the package that was replaced.
        try finishCommitted()
        return swap ? .updated : .installed
    }

    // MARK: - Recovery

    /// Finishes or undoes an install that a crash interrupted.
    ///
    /// The journal says how far the install got. A package that was swapped
    /// in stays; a complete staged package is promoted when the target is
    /// empty or still the managed package it was to replace; anything else
    /// that was staged is removed. The target is never removed, and nothing
    /// outside the installer's own directory is.
    func recover() throws {
        guard fileType(workDirectory.path) == .directory else { return }
        try removeOwned(journalTemporary)
        guard let record = readJournal() else {
            // Without a journal, or with one that cannot be read, nothing
            // staged can be trusted; the target was never touched by a
            // deletion, so dropping the staging is safe.
            try removeOwned(staging)
            try removeOwned(journal)
            return
        }
        switch record.phase {
        case .committed:
            try finishCommitted()
        case .staging:
            if verifiedDigest(of: target) == record.digest {
                // The swap happened; the staging path holds the old package.
                try writeJournal(Journal(version: record.version, digest: record.digest, phase: .committed))
                try finishCommitted()
                return
            }
            let stagedComplete = verifiedDigest(of: staging) == record.digest
            let targetState = fileType(target.path)
            let targetReplaceable = targetState == .absent
                || (targetState == .directory && verifiedDigest(of: target) != nil)
            if stagedComplete && targetReplaceable {
                let flags = UInt32(targetState == .absent ? RENAME_EXCL : RENAME_SWAP)
                if renamex_np(staging.path, target.path, flags) == 0 {
                    try syncDirectory(skillsRoot)
                    try syncDirectory(workDirectory)
                    try writeJournal(Journal(version: record.version, digest: record.digest, phase: .committed))
                    try finishCommitted()
                    return
                }
            }
            try removeOwned(staging)
            try removeOwned(journal)
            try syncDirectory(workDirectory)
        }
    }

    /// Removes the replaced package and the journal of a committed install.
    private func finishCommitted() throws {
        try removeOwned(staging)
        try removeOwned(journal)
        try syncDirectory(workDirectory)
    }

    // MARK: - The bundled package

    private struct PackageFile {
        let path: String
        let mode: UInt32
        let contents: Data
    }

    private struct BundledPackage {
        let version: SemanticVersion
        let digest: String
        let files: [PackageFile]
    }

    private func readBundledPackage() throws -> BundledPackage {
        let files: [PackageFile]
        do {
            files = try Self.readFiles(in: bundledPackage)
        } catch let error as PackageReadError {
            throw GuestSkillInstallError.invalidPackage("the bundled Skill \(error.message)")
        }
        if files.contains(where: { $0.path == Self.manifestName }) {
            throw GuestSkillInstallError.invalidPackage("the bundled Skill carries its own install manifest")
        }
        guard files.contains(where: { $0.path == "SKILL.md" }) else {
            throw GuestSkillInstallError.invalidPackage("the bundled Skill has no SKILL.md")
        }
        guard let versionFile = files.first(where: { $0.path == "VERSION" }),
              let version = SemanticVersion(versionFile: versionFile.contents) else {
            throw GuestSkillInstallError.invalidPackage("the bundled Skill has no valid VERSION")
        }
        return BundledPackage(version: version, digest: Self.digest(of: files), files: files)
    }

    private struct PackageReadError: Error {
        let message: String
    }

    /// Every regular file below `root`, with its normalized relative path.
    ///
    /// A symbolic link, a device, a socket, or any other file that is not a
    /// regular file or a directory makes the package invalid, as does a
    /// path that is not a plain relative path or that two files share after
    /// NFC normalization.
    private static func readFiles(in root: URL) throws -> [PackageFile] {
        var files: [PackageFile] = []
        var seen = Set<[UInt8]>()
        func walk(_ directory: String, relative: String) throws {
            let names: [String]
            do {
                names = try FileManager.default.contentsOfDirectory(atPath: directory)
            } catch {
                throw PackageReadError(message: "has a directory that cannot be read")
            }
            for name in names {
                let normalized = name.precomposedStringWithCanonicalMapping
                guard isPlainComponent(normalized) else {
                    throw PackageReadError(message: "has a file name that is not a plain path component")
                }
                let path = relative.isEmpty ? normalized : relative + "/" + normalized
                let full = directory + "/" + name
                var info = stat()
                guard lstat(full, &info) == 0 else {
                    throw PackageReadError(message: "has a file that cannot be read")
                }
                switch info.st_mode & S_IFMT {
                case S_IFDIR:
                    try walk(full, relative: path)
                case S_IFREG:
                    guard seen.insert(Array(path.utf8)).inserted else {
                        throw PackageReadError(message: "has two files with the same normalized path")
                    }
                    guard let contents = FileManager.default.contents(atPath: full) else {
                        throw PackageReadError(message: "has a file that cannot be read")
                    }
                    files.append(PackageFile(path: path, mode: UInt32(info.st_mode) & 0o7777, contents: contents))
                default:
                    throw PackageReadError(message: "has a file that is not a regular file")
                }
            }
        }
        var info = stat()
        guard lstat(root.path, &info) == 0, info.st_mode & S_IFMT == S_IFDIR else {
            throw PackageReadError(message: "is not a directory")
        }
        try walk(root.path, relative: "")
        return files.sorted { $0.path.utf8.lexicographicallyPrecedes($1.path.utf8) }
    }

    private static func isPlainComponent(_ name: String) -> Bool {
        !name.isEmpty && name != "." && name != ".."
            && !name.contains("/") && !name.contains("\\") && !name.unicodeScalars.contains("\u{0}")
    }

    /// The package digest of `files`, the install manifest excluded.
    ///
    /// SHA-256 over `leg.skill.package/v1` and one NUL byte, then one record
    /// per file in the order of the UTF-8 bytes of its path: the path length
    /// (u64, big-endian), the path, the permission bits (u32, big-endian),
    /// the content length (u64, big-endian), and the content.
    static func digest(of files: [(path: String, mode: UInt32, contents: Data)]) -> String {
        var context = sha256_ctx()
        sha256_init(&context)
        func feed(_ bytes: [UInt8]) {
            bytes.withUnsafeBytes { raw in
                if let base = raw.baseAddress { sha256_update(&context, base, raw.count) }
            }
        }
        func bigEndian<T: FixedWidthInteger>(_ value: T) -> [UInt8] {
            withUnsafeBytes(of: value.bigEndian) { Array($0) }
        }
        feed(Array(digestDomain.utf8) + [0])
        let sorted = files
            .filter { $0.path != manifestName }
            .sorted { $0.path.utf8.lexicographicallyPrecedes($1.path.utf8) }
        for file in sorted {
            let path = Array(file.path.utf8)
            feed(bigEndian(UInt64(path.count)))
            feed(path)
            feed(bigEndian(file.mode & 0o7777))
            feed(bigEndian(UInt64(file.contents.count)))
            feed([UInt8](file.contents))
        }
        var out = [UInt8](repeating: 0, count: 32)
        sha256_final(&context, &out)
        return out.map { String(format: "%02x", $0) }.joined()
    }

    private static func digest(of files: [PackageFile]) -> String {
        digest(of: files.map { (path: $0.path, mode: $0.mode, contents: $0.contents) })
    }

    // MARK: - The managed package

    private struct Manifest: Equatable {
        let version: SemanticVersion
        let digest: String

        var json: BridgeJSON {
            .object([
                "schema": .string(GuestSkillInstaller.manifestSchema),
                "owner": .string(GuestSkillInstaller.owner),
                "package": .string(GuestSkillInstaller.packageName),
                "version": .string(version.description),
                "digest_algorithm": .string("sha256"),
                "digest": .string(digest),
            ])
        }

        /// Reads a manifest, which must be exactly the canonical bytes of
        /// the six fields with the installer's values.
        init?(data: Data) {
            guard let value = try? BridgeCanonicalJSON.parse(data),
                  BridgeCanonicalJSON.encode(value) == data,
                  let keys = value.keys,
                  Set(keys) == ["schema", "owner", "package", "version", "digest_algorithm", "digest"],
                  value["schema"]?.stringValue == GuestSkillInstaller.manifestSchema,
                  value["owner"]?.stringValue == GuestSkillInstaller.owner,
                  value["package"]?.stringValue == GuestSkillInstaller.packageName,
                  value["digest_algorithm"]?.stringValue == "sha256",
                  let versionText = value["version"]?.stringValue,
                  let version = SemanticVersion(versionText),
                  let digest = value["digest"]?.stringValue,
                  BridgeValidation.isChecksum(digest) else { return nil }
            self.version = version
            self.digest = digest
        }

        init(version: SemanticVersion, digest: String) {
            self.version = version
            self.digest = digest
        }
    }

    /// The manifest of a package the installer owns, or a conflict.
    private func managedManifest(of package: URL) throws -> Manifest {
        guard let manifest = readManifest(of: package) else {
            throw GuestSkillInstallError.conflict(
                "the Skill target has no install manifest of this installer; it was left as it is")
        }
        guard verifiedDigest(of: package) == manifest.digest else {
            throw GuestSkillInstallError.conflict(
                "the Skill target no longer matches its install manifest; it was left as it is")
        }
        return manifest
    }

    private func readManifest(of package: URL) -> Manifest? {
        let path = package.appendingPathComponent(Self.manifestName).path
        guard fileType(path) == .regular,
              let attributes = try? FileManager.default.attributesOfItem(atPath: path),
              let size = attributes[.size] as? NSNumber, size.intValue <= 4096,
              let data = FileManager.default.contents(atPath: path) else { return nil }
        return Manifest(data: data)
    }

    /// The digest of `package` when its manifest is valid and its files
    /// still hash to the digest the manifest names.
    private func verifiedDigest(of package: URL) -> String? {
        guard fileType(package.path) == .directory,
              let manifest = readManifest(of: package),
              let files = try? Self.readFiles(in: package),
              Self.digest(of: files) == manifest.digest else { return nil }
        return manifest.digest
    }

    // MARK: - Staging

    private func checkSpace(for package: BundledPackage) throws {
        let needed = package.files.reduce(Int64(64 * 1024)) { $0 + Int64($1.contents.count) + 4096 }
        var info = statfs()
        let available: Int64
        if faults.contains(.space) {
            available = 0
        } else if statfs(workDirectory.path, &info) == 0 {
            available = Int64(info.f_bavail) * Int64(info.f_bsize)
        } else {
            throw GuestSkillInstallError.io("the free space could not be read")
        }
        guard available >= needed else {
            throw GuestSkillInstallError(reason: "insufficient_space",
                                         message: "the device has too little free space for the Skill")
        }
    }

    private func stage(_ package: BundledPackage, manifest: Manifest) throws {
        try removeOwned(staging)
        guard mkdir(staging.path, 0o755) == 0 else {
            throw GuestSkillInstallError.io("the staging directory could not be created")
        }
        for (index, file) in package.files.enumerated() {
            let url = staging.appendingPathComponent(file.path)
            try ensureDirectory(url.deletingLastPathComponent())
            try writeFile(url, contents: file.contents, mode: file.mode)
            if index == 0 && faults.contains(.copy) {
                throw GuestSkillInstallError.io("injected failure while copying")
            }
        }
        let manifestURL = staging.appendingPathComponent(Self.manifestName)
        if faults.contains(.manifest) {
            // A crash after the manifest bytes were written, before they were
            // made durable: the manifest is cut short.
            let bytes = BridgeCanonicalJSON.encode(manifest.json)
            try writeFile(manifestURL, contents: bytes.prefix(bytes.count / 2), mode: 0o644)
            throw GuestSkillInstallError.io("injected failure while writing the manifest")
        }
        try writeFile(manifestURL, contents: BridgeCanonicalJSON.encode(manifest.json), mode: 0o644)
        try syncTree(staging)
        try syncDirectory(workDirectory)

        var stagedDigest = verifiedDigest(of: staging)
        if faults.contains(.hash) { stagedDigest = String(repeating: "0", count: 64) }
        guard stagedDigest == package.digest else {
            throw GuestSkillInstallError.io("the staged Skill does not match the bundled digest")
        }
    }

    // MARK: - The journal

    private enum Phase: String {
        case staging
        case committed
    }

    private struct Journal {
        let version: SemanticVersion
        let digest: String
        let phase: Phase
    }

    private func writeJournal(_ record: Journal) throws {
        let value = BridgeJSON.object([
            "schema": .string(Self.journalSchema),
            "package": .string(Self.packageName),
            "version": .string(record.version.description),
            "digest": .string(record.digest),
            "phase": .string(record.phase.rawValue),
        ])
        // The journal is replaced in one rename, so a crash leaves the old
        // record or the new one, never half of one.
        try removeOwned(journalTemporary)
        try writeFile(journalTemporary, contents: BridgeCanonicalJSON.encode(value), mode: 0o644)
        guard rename(journalTemporary.path, journal.path) == 0 else {
            throw GuestSkillInstallError.io("the install journal could not be written")
        }
        try syncDirectory(workDirectory)
    }

    private func readJournal() -> Journal? {
        guard fileType(journal.path) == .regular,
              let data = FileManager.default.contents(atPath: journal.path), data.count <= 4096,
              let value = try? BridgeCanonicalJSON.parse(data),
              value["schema"]?.stringValue == Self.journalSchema,
              value["package"]?.stringValue == Self.packageName,
              let version = value["version"]?.stringValue.flatMap(SemanticVersion.init),
              let digest = value["digest"]?.stringValue, BridgeValidation.isChecksum(digest),
              let phase = value["phase"]?.stringValue.flatMap(Phase.init) else { return nil }
        return Journal(version: version, digest: digest, phase: phase)
    }

    // MARK: - Files

    private enum FileType: Equatable {
        case absent
        case directory
        case regular
        case other
    }

    /// The type of the file at `path`, without following a symbolic link.
    private func fileType(_ path: String) -> FileType {
        var info = stat()
        guard lstat(path, &info) == 0 else { return .absent }
        switch info.st_mode & S_IFMT {
        case S_IFDIR: return .directory
        case S_IFREG: return .regular
        default: return .other
        }
    }

    /// Creates `directory` and its parents; every existing one must be a real
    /// directory, so a link cannot lead the install out of the guest home.
    private func ensureDirectory(_ directory: URL) throws {
        switch fileType(directory.path) {
        case .directory:
            return
        case .absent:
            try ensureDirectory(directory.deletingLastPathComponent())
            guard mkdir(directory.path, 0o755) == 0 || errno == EEXIST,
                  fileType(directory.path) == .directory else {
                throw GuestSkillInstallError.io("a directory for the Skill could not be created")
            }
        case .regular, .other:
            throw GuestSkillInstallError.conflict(
                "\(directory.lastPathComponent) is not a directory; it was left as it is")
        }
    }

    private func writeFile(_ url: URL, contents: Data, mode: UInt32) throws {
        let descriptor = open(url.path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600)
        guard descriptor >= 0 else {
            throw GuestSkillInstallError.io("a Skill file could not be created")
        }
        defer { close(descriptor) }
        let written = contents.withUnsafeBytes { raw -> Bool in
            var offset = 0
            while offset < raw.count {
                let count = write(descriptor, raw.baseAddress! + offset, raw.count - offset)
                if count < 0 {
                    if errno == EINTR { continue }
                    return false
                }
                offset += count
            }
            return true
        }
        // The mode is set after the file exists, so the umask cannot change it.
        guard written, fchmod(descriptor, mode_t(mode & 0o7777)) == 0, fsync(descriptor) == 0 else {
            throw GuestSkillInstallError.io("a Skill file could not be written")
        }
    }

    private func syncDirectory(_ directory: URL) throws {
        let descriptor = open(directory.path, O_RDONLY | O_DIRECTORY)
        guard descriptor >= 0 else {
            throw GuestSkillInstallError.io("a Skill directory could not be opened")
        }
        defer { close(descriptor) }
        guard fsync(descriptor) == 0 else {
            throw GuestSkillInstallError.io("a Skill directory could not be synchronized")
        }
    }

    private func syncTree(_ directory: URL) throws {
        let names = (try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []
        for name in names {
            let child = directory.appendingPathComponent(name)
            if fileType(child.path) == .directory { try syncTree(child) }
        }
        try syncDirectory(directory)
    }

    /// Removes a file or directory below the installer's own directory.
    private func removeOwned(_ url: URL) throws {
        precondition(url.path.hasPrefix(workDirectory.path + "/"), "the installer removes only what it staged")
        guard fileType(url.path) != .absent else { return }
        do {
            try FileManager.default.removeItem(at: url)
        } catch {
            throw GuestSkillInstallError.io("a staged Skill file could not be removed")
        }
    }
}

/// A `major.minor.patch` version without a pre-release or build part.
struct SemanticVersion: Comparable, CustomStringConvertible {
    let major: UInt64
    let minor: UInt64
    let patch: UInt64

    init?(_ text: String) {
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3 else { return nil }
        var numbers: [UInt64] = []
        for part in parts {
            // SemVer forbids a leading zero, and the bound keeps the text short.
            guard (1...19).contains(part.count), part.allSatisfy({ $0.isASCII && $0.isNumber }),
                  part == "0" || !part.hasPrefix("0"), let number = UInt64(part) else { return nil }
            numbers.append(number)
        }
        (major, minor, patch) = (numbers[0], numbers[1], numbers[2])
    }

    /// Reads a VERSION file: one ASCII line, with or without its newline.
    init?(versionFile: Data) {
        guard versionFile.allSatisfy({ $0 < 0x80 }) else { return nil }
        var text = String(decoding: versionFile, as: UTF8.self)
        if text.hasSuffix("\n") { text.removeLast() }
        self.init(text)
    }

    var description: String { "\(major).\(minor).\(patch)" }

    static func < (left: SemanticVersion, right: SemanticVersion) -> Bool {
        (left.major, left.minor, left.patch) < (right.major, right.minor, right.patch)
    }
}
