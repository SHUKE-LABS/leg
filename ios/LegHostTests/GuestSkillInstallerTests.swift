import XCTest
@testable import LegHost

/// The transactional install of the bundled Skill.
///
/// Every test runs the real installer against a scratch bundle and a scratch
/// guest root, so the renames, the journal, and the recovery are the ones the
/// app runs.
final class GuestSkillInstallerTests: XCTestCase {
    private var scratch: URL!
    private var bundle: URL!
    private var activeRoot: URL!

    override func setUpWithError() throws {
        scratch = FileManager.default.temporaryDirectory
            .appendingPathComponent("skill-installer-\(UUID().uuidString)", isDirectory: true)
        bundle = scratch.appendingPathComponent("bundle/mobile-media-files", isDirectory: true)
        activeRoot = scratch.appendingPathComponent("root", isDirectory: true)
        try FileManager.default.createDirectory(at: activeRoot.appendingPathComponent("data/root"),
                                                withIntermediateDirectories: true)
        try writeBundle(version: "1.0.0", body: "first")
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: scratch)
    }

    // MARK: - Helpers

    private func writeBundle(version: String, body: String) throws {
        try? FileManager.default.removeItem(at: bundle)
        try FileManager.default.createDirectory(at: bundle.appendingPathComponent("references"),
                                                withIntermediateDirectories: true)
        try Data("---\nname: mobile-media-files\ndescription: test\n---\n\(body)\n".utf8)
            .write(to: bundle.appendingPathComponent("SKILL.md"))
        try Data("\(version)\n".utf8).write(to: bundle.appendingPathComponent("VERSION"))
        try Data("guide\n".utf8).write(to: bundle.appendingPathComponent("references/guide.md"))
    }

    private func installer(_ faults: Set<GuestSkillInstaller.Fault> = []) -> GuestSkillInstaller {
        let installer = GuestSkillInstaller(bundledPackage: bundle, activeRoot: activeRoot)
        installer.faults = faults
        return installer
    }

    private var target: URL {
        activeRoot.appendingPathComponent("data/root/.agents/skills/mobile-media-files", isDirectory: true)
    }

    private var workDirectory: URL {
        activeRoot.appendingPathComponent("data/root/.agents/.leg-skill-install", isDirectory: true)
    }

    private func installedText(_ name: String) throws -> String {
        String(decoding: try Data(contentsOf: target.appendingPathComponent(name)), as: UTF8.self)
    }

    private func manifest() throws -> BridgeJSON {
        try BridgeCanonicalJSON.parse(Data(contentsOf: target.appendingPathComponent(".leg-manifest.json")))
    }

    /// The installer leaves nothing of its own behind once an install ended.
    private func assertNoStaging(file: StaticString = #filePath, line: UInt = #line) {
        let leftovers = (try? FileManager.default.contentsOfDirectory(atPath: workDirectory.path)) ?? []
        XCTAssertEqual(leftovers, [], file: file, line: line)
    }

    private func assertInstalled(version: String, body: String,
                                 file: StaticString = #filePath, line: UInt = #line) throws {
        XCTAssertTrue(try installedText("SKILL.md").contains(body), file: file, line: line)
        XCTAssertEqual(try installedText("VERSION"), "\(version)\n", file: file, line: line)
        XCTAssertEqual(try manifest()["version"]?.stringValue, version, file: file, line: line)
        assertNoStaging(file: file, line: line)
    }

    // MARK: - The digest

    func testThePackageDigestMatchesTheReferenceVector() {
        // Computed independently: SHA-256 over "leg.skill.package/v1\0" and
        // one record per file in UTF-8 path order.
        let digest = GuestSkillInstaller.digest(of: [
            (path: "VERSION", mode: 0o644, contents: Data("1.0.0\n".utf8)),
            (path: "references/guide.md", mode: 0o600, contents: Data("guide\n".utf8)),
            (path: "SKILL.md", mode: 0o644, contents: Data("skill\n".utf8)),
            (path: ".leg-manifest.json", mode: 0o644, contents: Data("ignored".utf8)),
        ])
        XCTAssertEqual(digest, "fbd5614b9828fa8557937dc0ecf0668d7c25caef58cdbbb8021ed7a3b692fea1")
    }

    // MARK: - Install, no-op, upgrade, downgrade

    func testAFreshInstallWritesTheCanonicalManifest() throws {
        XCTAssertEqual(try installer().install(), .installed)
        try assertInstalled(version: "1.0.0", body: "first")
        let data = try Data(contentsOf: target.appendingPathComponent(".leg-manifest.json"))
        let value = try BridgeCanonicalJSON.parse(data)
        XCTAssertEqual(BridgeCanonicalJSON.encode(value), data)
        XCTAssertEqual(Set(value.keys ?? []), ["schema", "owner", "package", "version", "digest_algorithm", "digest"])
        XCTAssertEqual(value["schema"]?.stringValue, "leg.skill-install/v1")
        XCTAssertEqual(value["owner"]?.stringValue, "leg-ios-bundled")
        XCTAssertEqual(value["package"]?.stringValue, "mobile-media-files")
        XCTAssertEqual(value["digest_algorithm"]?.stringValue, "sha256")
        XCTAssertTrue(BridgeValidation.isChecksum(value["digest"]?.stringValue ?? ""))
    }

    func testASecondInstallIsANoOp() throws {
        try installer().install()
        let before = try Data(contentsOf: target.appendingPathComponent(".leg-manifest.json"))
        XCTAssertEqual(try installer().install(), .unchanged)
        XCTAssertEqual(try Data(contentsOf: target.appendingPathComponent(".leg-manifest.json")), before)
        assertNoStaging()
    }

    func testAHigherBundledVersionReplacesTheManagedPackage() throws {
        try installer().install()
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertEqual(try installer().install(), .updated)
        try assertInstalled(version: "1.1.0", body: "second")
    }

    func testALowerBundledVersionIsANoOp() throws {
        try writeBundle(version: "2.0.0", body: "newer")
        try installer().install()
        try writeBundle(version: "1.9.9", body: "older")
        XCTAssertEqual(try installer().install(), .olderBundled)
        try assertInstalled(version: "2.0.0", body: "newer")
    }

    // MARK: - Conflicts

    func testTheSameVersionWithAnotherDigestIsAConflict() throws {
        try installer().install()
        try writeBundle(version: "1.0.0", body: "changed")
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "install_conflict")
        }
        try assertInstalled(version: "1.0.0", body: "first")
    }

    func testAUserPackageAtTheTargetIsPreserved() throws {
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("user skill\n".utf8).write(to: target.appendingPathComponent("SKILL.md"))
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "install_conflict")
        }
        XCTAssertEqual(try installedText("SKILL.md"), "user skill\n")
        XCTAssertFalse(FileManager.default.fileExists(atPath: target.appendingPathComponent(".leg-manifest.json").path))
    }

    func testAManagedPackageTheUserEditedIsPreserved() throws {
        try installer().install()
        try Data("edited\n".utf8).write(to: target.appendingPathComponent("SKILL.md"))
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "install_conflict")
        }
        XCTAssertEqual(try installedText("SKILL.md"), "edited\n")
    }

    func testAForeignManifestIsAConflict() throws {
        try installer().install()
        let manifestURL = target.appendingPathComponent(".leg-manifest.json")
        let text = String(decoding: try Data(contentsOf: manifestURL), as: UTF8.self)
        try Data(text.replacingOccurrences(of: "leg-ios-bundled", with: "someone-else").utf8).write(to: manifestURL)
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "install_conflict")
        }
        XCTAssertTrue(try installedText("SKILL.md").contains("first"))
    }

    // MARK: - The bundled package

    func testASymbolicLinkInTheBundleIsRefused() throws {
        try FileManager.default.createSymbolicLink(atPath: bundle.appendingPathComponent("escape").path,
                                                   withDestinationPath: "/etc/hosts")
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "invalid_package")
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: target.path))
    }

    func testAVersionThatIsNotSemVerIsRefused() throws {
        try writeBundle(version: "1.0", body: "first")
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "invalid_package")
        }
    }

    func testAGuestLinkInPlaceOfTheSkillsRootIsRefused() throws {
        // The guest's emulated file system stores a link as a regular file.
        let agents = activeRoot.appendingPathComponent("data/root/.agents")
        try Data("/".utf8).write(to: agents)
        XCTAssertThrowsError(try installer().install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "install_conflict")
        }
        XCTAssertEqual(try Data(contentsOf: agents), Data("/".utf8))
    }

    // MARK: - Interrupted installs

    /// Runs an upgrade that fails at `fault`, then the recovery and a clean
    /// install, and checks that the result is the new package.
    private func assertUpgradeRecovers(from fault: GuestSkillInstaller.Fault,
                                       expectsOldAfterFailure: Bool,
                                       file: StaticString = #filePath, line: UInt = #line) throws {
        try installer().install()
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertThrowsError(try installer([fault]).install(), file: file, line: line)
        if expectsOldAfterFailure {
            XCTAssertTrue(try installedText("SKILL.md").contains("first"), file: file, line: line)
        }
        try installer().recover()
        assertNoStaging(file: file, line: line)
        XCTAssertNoThrow(try installer().install(), file: file, line: line)
        try assertInstalled(version: "1.1.0", body: "second", file: file, line: line)
    }

    func testAFailureWhileCopyingKeepsTheOldPackageAndRecovers() throws {
        try assertUpgradeRecovers(from: .copy, expectsOldAfterFailure: true)
    }

    func testTooLittleSpaceKeepsTheOldPackage() throws {
        try installer().install()
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertThrowsError(try installer([.space]).install()) { error in
            XCTAssertEqual((error as? GuestSkillInstallError)?.reason, "insufficient_space")
        }
        try assertInstalled(version: "1.0.0", body: "first")
        XCTAssertEqual(try installer().install(), .updated)
    }

    func testAHashMismatchKeepsTheOldPackageAndRecovers() throws {
        try assertUpgradeRecovers(from: .hash, expectsOldAfterFailure: true)
    }

    func testACrashRightAfterTheSwapKeepsTheNewPackage() throws {
        try installer().install()
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertThrowsError(try installer([.swap]).install())
        // The swap happened; the journal has not recorded it yet.
        XCTAssertTrue(try installedText("SKILL.md").contains("second"))
        try installer().recover()
        try assertInstalled(version: "1.1.0", body: "second")
        XCTAssertEqual(try installer().install(), .unchanged)
    }

    func testACutShortManifestKeepsTheOldPackageAndRecovers() throws {
        try assertUpgradeRecovers(from: .manifest, expectsOldAfterFailure: true)
    }

    func testAFailedFirstInstallLeavesNoTargetAndRecovers() throws {
        XCTAssertThrowsError(try installer([.copy]).install())
        XCTAssertFalse(FileManager.default.fileExists(atPath: target.path))
        XCTAssertEqual(try installer().install(), .installed)
        try assertInstalled(version: "1.0.0", body: "first")
    }

    func testRecoveryNeverTouchesTheUserWorkspace() throws {
        let home = activeRoot.appendingPathComponent("data/root")
        let marker = home.appendingPathComponent("workspace/marker.txt")
        let userSkill = home.appendingPathComponent(".agents/skills/mine/SKILL.md")
        try FileManager.default.createDirectory(at: marker.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: userSkill.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("keep".utf8).write(to: marker)
        try Data("mine".utf8).write(to: userSkill)

        try installer().install()
        try writeBundle(version: "1.1.0", body: "second")
        XCTAssertThrowsError(try installer([.swap]).install())
        try installer().recover()
        try installer().install()

        XCTAssertEqual(try Data(contentsOf: marker), Data("keep".utf8))
        XCTAssertEqual(try Data(contentsOf: userSkill), Data("mine".utf8))
    }
}
