import Foundation

#if canImport(Darwin)
import Darwin
#endif

/// The state of the runtime files of the host.
///
/// The three states are separate because they ask different things of the
/// user: the files have to be there, they have to be the pinned ones, and
/// the container has to hold the space the import of the guest root needs.
enum HostRuntimeFiles: Equatable {
    /// The pinned runtime files are not in the app bundle.
    case absent
    /// The pinned runtime files are there, but one of them is not the file
    /// that was pinned.
    case corrupt
    /// The pinned runtime files are there and they are the pinned ones.
    case verified
}

/// The architecture the host runs on.
///
/// The pinned guest bundle is an i386 Linux program and runs on the vendored
/// emulator, which the host carries for the architectures the app is built
/// for. A host on any other architecture cannot run it.
enum HostArchitecture: Equatable {
    /// The host runs on an architecture the pinned guest runtime supports.
    case supported
    /// The host runs on an architecture the pinned guest runtime does not
    /// support.
    case unsupported(String)
}

/// A failure the host reports before it touches the guest root.
///
/// Every case carries a stable identifier and a concrete remediation, so the
/// user knows what to do and the tests can assert on the identifier.
enum HostPreflightDiagnostic: Error, LocalizedError, Equatable {
    /// The pinned runtime files are not in the app bundle.
    case runtimeFilesMissing
    /// One of the pinned runtime files is not the file that was pinned.
    case runtimeFilesCorrupt
    /// The host runs on an architecture the pinned guest runtime does not
    /// support.
    case architectureUnsupported(String)
    /// The app container does not hold the space the import needs.
    case storageInsufficient(available: Int64, required: Int64)

    /// The stable identifier of the diagnostic.
    var identifier: String {
        switch self {
        case .runtimeFilesMissing:
            return "IOS_RUNTIME_FILES_MISSING"
        case .runtimeFilesCorrupt:
            return "IOS_RUNTIME_FILES_CORRUPT"
        case .architectureUnsupported:
            return "IOS_ARCHITECTURE_UNSUPPORTED"
        case .storageInsufficient:
            return "IOS_STORAGE_INSUFFICIENT"
        }
    }

    var errorDescription: String? {
        switch self {
        case .runtimeFilesMissing:
            return "\(identifier): the pinned guest runtime files are not in the app bundle. "
                + "Remove the app from the device and install it again, so the pinned inputs "
                + "are staged and verified again."
        case .runtimeFilesCorrupt:
            return "\(identifier): one of the pinned guest runtime files does not match its "
                + "pinned digest. Remove the app from the device and install it again, so the "
                + "pinned inputs are downloaded and verified again."
        case .architectureUnsupported(let architecture):
            return "\(identifier): the host runs on \(architecture), which the pinned guest "
                + "runtime does not support. The pinned guest bundle is an i386 Linux program "
                + "and runs on an x86_64 or an arm64 host."
        case .storageInsufficient(let available, let required):
            return "\(identifier): the app container holds \(available) bytes of free space and "
                + "the import of the guest root needs \(required) bytes. Free space on the "
                + "device and start the host again."
        }
    }
}

/// The providers the preflight reads the state of the device from.
///
/// The providers are injected, so the checks can be exercised without a
/// device: a test hands in the state it wants the preflight to see.
struct HostPreflightProviders {
    /// The state of the pinned runtime files.
    var runtimeFiles: () -> HostRuntimeFiles
    /// The free space of the app container, in bytes.
    var freeStorageBytes: () -> Int64
    /// The architecture the host runs on.
    var architecture: () -> HostArchitecture
}

/// The checks the host makes before it touches the guest root.
///
/// The checks run in a fixed order and the first one that fails is the
/// diagnostic the host shows. The order is the order the user has to act
/// in: the files have to be there before their digest means anything, the
/// architecture decides whether the guest can run at all, and only then
/// does the space of the import matter.
enum HostPreflight {
    /// The space the import of the guest root needs, in bytes.
    ///
    /// The pinned Alpine root and the guest bundle expand into a root of
    /// about 200 MB, and the importer keeps the merged staging archive next
    /// to it while it works, so the container needs half a gigabyte of free
    /// space for the import.
    static let requiredStorageBytes: Int64 = 512 * 1024 * 1024

    /// The architectures the pinned guest runtime runs on.
    static let supportedArchitectures = ["x86_64", "arm64"]

    /// Checks the device and reports the first failure.
    ///
    /// The check throws the diagnostic of the first failure, so the caller
    /// reports exactly one of them.
    static func check(_ providers: HostPreflightProviders) throws {
        switch providers.runtimeFiles() {
        case .absent:
            throw HostPreflightDiagnostic.runtimeFilesMissing
        case .corrupt:
            throw HostPreflightDiagnostic.runtimeFilesCorrupt
        case .verified:
            break
        }
        if case .unsupported(let architecture) = providers.architecture() {
            throw HostPreflightDiagnostic.architectureUnsupported(architecture)
        }
        let available = providers.freeStorageBytes()
        guard available >= requiredStorageBytes else {
            throw HostPreflightDiagnostic.storageInsufficient(available: available,
                                                               required: requiredStorageBytes)
        }
    }
}

extension HostPreflightProviders {
    /// The providers of one run of the host.
    ///
    /// The DEBUG launch controls replace the state of the device for one
    /// run, so the faults the preflight reports can be induced in the
    /// simulator without breaking the device. A build without the controls
    /// reads the device itself.
    static func live(options: HostLaunchOptions, pins: HostPins?) -> HostPreflightProviders {
        HostPreflightProviders(
            runtimeFiles: {
                #if DEBUG
                if options.runtimeFilesMissing { return .absent }
                if options.runtimeFilesCorrupt { return .corrupt }
                #endif
                guard let pins else { return .absent }
                return HostPreflightProviders.state(of: pins)
            },
            freeStorageBytes: {
                #if DEBUG
                if options.lowStorage { return HostPreflight.requiredStorageBytes - 1 }
                #endif
                return HostPreflightProviders.freeStorageBytes()
            },
            architecture: {
                #if DEBUG
                if options.unsupportedArchitecture { return .unsupported("mips64") }
                #endif
                return HostPreflightProviders.hostArchitecture()
            }
        )
    }

    /// The state of the pinned runtime files.
    ///
    /// A pinned input that is not in the app bundle is missing, and a pinned
    /// input whose digest does not match the pin is corrupt. The digest of
    /// the files is read here as well as in the provisioner, because the
    /// preflight has to report the fault before the guest root is touched.
    static func state(of pins: HostPins) -> HostRuntimeFiles {
        for input in pins.inputs {
            guard let url = try? HostPins.embeddedInputURL(named: input.filename),
                  FileManager.default.fileExists(atPath: url.path)
            else {
                return .absent
            }
            guard (try? HostPreflightProviders.digest(of: url)) == input.sha256 else {
                return .corrupt
            }
        }
        return .verified
    }

    /// The digest of a file, in lowercase hex.
    static func digest(of url: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        var context = sha256_ctx()
        sha256_init(&context)
        while true {
            let chunk = try handle.read(upToCount: 1024 * 1024) ?? Data()
            if chunk.isEmpty { break }
            chunk.withUnsafeBytes { raw in
                guard let base = raw.baseAddress else { return }
                sha256_update(&context, base, raw.count)
            }
        }
        var bytes = [UInt8](repeating: 0, count: Int(SHA256_DIGEST_LEN))
        sha256_final(&context, &bytes)
        return bytes.map { String(format: "%02x", Int($0)) }.joined()
    }

    /// The free space of the app container, in bytes.
    ///
    /// The capacity that cannot be read is not a shortage: the check reports
    /// a shortage only when the container is known to be too small.
    static func freeStorageBytes() -> Int64 {
        let support = (try? FileManager.default.url(for: .applicationSupportDirectory,
                                                    in: .userDomainMask,
                                                    appropriateFor: nil,
                                                    create: true))
            ?? FileManager.default.urls(for: .applicationSupportDirectory,
                                        in: .userDomainMask).first
        guard let support else { return Int64.max }
        if let values = try? support.resourceValues(
            forKeys: [.volumeAvailableCapacityForImportantUsageKey]),
           let capacity = values.volumeAvailableCapacityForImportantUsage {
            return capacity
        }
        if let attributes = try? FileManager.default.attributesOfFileSystem(forPath: support.path),
           let free = attributes[.systemFreeSize] as? NSNumber {
            return free.int64Value
        }
        return Int64.max
    }

    /// The architecture the host runs on.
    static func hostArchitecture() -> HostArchitecture {
        #if canImport(Darwin)
        var system = utsname()
        uname(&system)
        let machine = withUnsafeBytes(of: &system.machine) { raw -> String in
            guard let base = raw.baseAddress else { return "" }
            return String(cString: base.assumingMemoryBound(to: CChar.self))
        }
        if HostPreflight.supportedArchitectures.contains(machine) {
            return .supported
        }
        return .unsupported(machine.isEmpty ? "an unknown architecture" : machine)
        #else
        return .supported
        #endif
    }
}
