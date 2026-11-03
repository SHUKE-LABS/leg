import XCTest
@testable import LegHost

/// The preflight of the host.
///
/// The test hands the preflight the state of a device it makes up, so the
/// order of the checks and the identifier of every diagnostic are checked
/// without a device.
final class HostPreflightTests: XCTestCase {
    /// The checks pass when the device is in order.
    func testADeviceInOrderPassesThePreflight() throws {
        XCTAssertNoThrow(try HostPreflight.check(Self.providers()))
    }

    /// The first failing check is the diagnostic the host shows.
    func testTheFirstFailingCheckWins() {
        XCTAssertEqual(Self.diagnostic(Self.providers(runtimeFiles: { .absent },
                                                     freeStorageBytes: { 0 },
                                                     architecture: { .unsupported("mips64") })),
                       "IOS_RUNTIME_FILES_MISSING")
        XCTAssertEqual(Self.diagnostic(Self.providers(runtimeFiles: { .corrupt },
                                                     freeStorageBytes: { 0 },
                                                     architecture: { .unsupported("mips64") })),
                       "IOS_RUNTIME_FILES_CORRUPT")
        XCTAssertEqual(Self.diagnostic(Self.providers(runtimeFiles: { .verified },
                                                     freeStorageBytes: { 0 },
                                                     architecture: { .unsupported("mips64") })),
                       "IOS_ARCHITECTURE_UNSUPPORTED")
        XCTAssertEqual(Self.diagnostic(Self.providers(runtimeFiles: { .verified },
                                                     freeStorageBytes: { 0 },
                                                     architecture: { .supported })),
                       "IOS_STORAGE_INSUFFICIENT")
    }

    /// The runtime files are checked before the architecture.
    func testTheRuntimeFilesComeBeforeTheArchitecture() {
        let providers = Self.providers(runtimeFiles: { .absent },
                                       freeStorageBytes: { HostPreflight.requiredStorageBytes },
                                       architecture: { .unsupported("mips64") })
        XCTAssertEqual(Self.diagnostic(providers), "IOS_RUNTIME_FILES_MISSING")
    }

    /// The architecture is checked before the space of the import.
    func testTheArchitectureComesBeforeTheSpace() {
        let providers = Self.providers(runtimeFiles: { .verified },
                                       freeStorageBytes: { 0 },
                                       architecture: { .unsupported("mips64") })
        XCTAssertEqual(Self.diagnostic(providers), "IOS_ARCHITECTURE_UNSUPPORTED")
    }

    /// The space of the import has to be there.
    func testTheSpaceOfTheImportIsChecked() {
        let providers = Self.providers(freeStorageBytes: { HostPreflight.requiredStorageBytes - 1 })
        XCTAssertEqual(Self.diagnostic(providers), "IOS_STORAGE_INSUFFICIENT")
    }

    /// Every diagnostic names itself and says what to do about it.
    func testEveryDiagnosticCarriesItsIdentifierAndARemediation() {
        let diagnostics: [HostPreflightDiagnostic] = [
            .runtimeFilesMissing,
            .runtimeFilesCorrupt,
            .architectureUnsupported("mips64"),
            .storageInsufficient(available: 0, required: HostPreflight.requiredStorageBytes),
        ]
        for diagnostic in diagnostics {
            let message = diagnostic.errorDescription ?? ""
            XCTAssertTrue(message.hasPrefix(diagnostic.identifier),
                          "the message of \(diagnostic.identifier) does not name it")
            XCTAssertTrue(message.count > diagnostic.identifier.count + 20,
                          "the message of \(diagnostic.identifier) holds no remediation")
        }
    }

    /// The identifiers of the diagnostics are stable.
    func testTheIdentifiersAreStable() {
        XCTAssertEqual(HostPreflightDiagnostic.runtimeFilesMissing.identifier,
                       "IOS_RUNTIME_FILES_MISSING")
        XCTAssertEqual(HostPreflightDiagnostic.runtimeFilesCorrupt.identifier,
                       "IOS_RUNTIME_FILES_CORRUPT")
        XCTAssertEqual(HostPreflightDiagnostic.architectureUnsupported("mips64").identifier,
                       "IOS_ARCHITECTURE_UNSUPPORTED")
        XCTAssertEqual(HostPreflightDiagnostic.storageInsufficient(available: 0, required: 1).identifier,
                       "IOS_STORAGE_INSUFFICIENT")
    }

    /// The providers of a device that is in order.
    private static func providers(runtimeFiles: @escaping () -> HostRuntimeFiles = { .verified },
                                  freeStorageBytes: @escaping () -> Int64 = {
                                      HostPreflight.requiredStorageBytes
                                  },
                                  architecture: @escaping () -> HostArchitecture = { .supported }
                                  ) -> HostPreflightProviders {
        HostPreflightProviders(runtimeFiles: runtimeFiles,
                               freeStorageBytes: freeStorageBytes,
                               architecture: architecture)
    }

    /// The identifier of the diagnostic the preflight reports.
    private static func diagnostic(_ providers: HostPreflightProviders) -> String? {
        do {
            try HostPreflight.check(providers)
            return nil
        } catch {
            return (error as? HostPreflightDiagnostic)?.identifier
        }
    }
}
