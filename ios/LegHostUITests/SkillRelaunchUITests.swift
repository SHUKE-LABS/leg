import XCTest

/// The bundled Skill in a running guest, across a relaunch.
///
/// The first launch installs the package where the guest discovers user
/// Skills; the second launch finds the same package and leaves it, and a
/// Skill the user made next to it survives.
final class SkillRelaunchUITests: XCTestCase {
    private let startTimeout: TimeInterval = 300
    private static let provider = ["-leghost-provider", "anthropic"]

    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
    }

    override func tearDownWithError() throws {
        app = nil
    }

    func testTheSkillIsInstalledOnceAndKeptAcrossARelaunch() {
        launchShell(["-leghost-reset"])
        XCTAssertTrue(waitForState("skill=installed"), "the Skill was not installed")

        // The quotes split the marker, so the echo of the command is not the
        // output it waits for.
        app.typeText("cat ~/.agents/skills/mobile-media-files/VERSION; "
            + "grep -c '^name: mobile-media-files' ~/.agents/skills/mobile-media-files/SKILL.md && echo SKILL''_FOUND\n")
        XCTAssertTrue(waitForOutput("SKILL_FOUND"), "the guest does not see the Skill")
        XCTAssertTrue(waitForOutput("1.0.0"), "the guest sees another version")

        app.typeText("mkdir -p ~/.agents/skills/my-note && echo '# mine' > ~/.agents/skills/my-note/SKILL.md "
            + "&& echo NOTE''_MADE\n")
        // The app is not stopped before the guest wrote the marker Skill.
        XCTAssertTrue(waitForOutput("NOTE_MADE"), "the guest did not make the marker Skill")

        app.terminate()
        launchShell([])
        XCTAssertTrue(waitForState("skill=unchanged"), "the relaunch did not keep the installed Skill")
        app.typeText("test -f ~/.agents/skills/my-note/SKILL.md && test -f ~/.agents/skills/mobile-media-files/.leg-manifest.json && echo BOTH''_KEPT\n")
        XCTAssertTrue(waitForOutput("BOTH_KEPT"), "the relaunch lost a Skill")
    }

    // MARK: Helpers

    private func launchShell(_ arguments: [String]) {
        app.launchArguments = arguments + Self.provider + ["-leghost-program", "/bin/sh"]
        app.launch()
        XCTAssertTrue(app.otherElements["guest-terminal"].waitForExistence(timeout: startTimeout),
                      "the terminal is missing")
        GuestKeyboard.focus(app)
    }

    private func waitForOutput(_ text: String, timeout: TimeInterval = 60) -> Bool {
        app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
            .waitForExistence(timeout: timeout)
    }

    private func waitForState(_ text: String, timeout: TimeInterval = 120) -> Bool {
        let state = app.staticTexts["guest-fixture-state"]
        guard state.waitForExistence(timeout: timeout) else { return false }
        let expectation = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS %@", text), object: state)
        return XCTWaiter().wait(for: [expectation], timeout: timeout) == .completed
    }
}
