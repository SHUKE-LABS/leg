import XCTest

/// Drives the LegHost app in the simulator.
///
/// The test covers the parts of the host that only show up in a running
/// simulator: the guest root is imported on the first launch, the guest
/// program runs in the terminal, the keyboard reaches it, a rotation changes
/// the size the guest was told about, an interrupt reaches the guest program,
/// and the second launch reuses the verified root instead of importing it
/// again.
final class LegHostUITests: XCTestCase {
    /// The host imports the guest root, which takes a moment on a cold
    /// container.
    private let startTimeout: TimeInterval = 300

    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        // The reset makes the test start from a known state: the host removes
        // the provisioned root and imports it again.
        app.launchArguments = ["-leghost-reset"]
        app.launch()
        // Every test starts in portrait, so a rotation has a direction to
        // change.
        XCUIDevice.shared.orientation = .portrait
    }

    override func tearDownWithError() throws {
        app = nil
    }

    /// The first launch imports the guest root and starts the guest program.
    func testTheFirstLaunchImportsTheGuestRoot() {
        let state = app.staticTexts["guest-root-state"]
        XCTAssertTrue(state.waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")
        XCTAssertEqual(state.label, "imported root")
        attachScreenshot("first launch")
    }

    /// The guest program runs in the terminal, the keyboard reaches it, and a
    /// rotation changes the size the guest was told about.
    func testTheTerminalTakesInputAndFollowsTheRotation() {
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        let terminal = app.otherElements["guest-terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal is missing")

        // The guest program is on the screen: it asks for a workspace.
        let prompt = app.staticTexts
            .matching(NSPredicate(format: "label CONTAINS %@", "workspace")).firstMatch
        XCTAssertTrue(prompt.waitForExistence(timeout: 60), "the guest program did not draw its screen")

        let portraitSize = settledTerminalSize()

        // The keyboard reaches the guest program. The tap gives the keyboard
        // to the terminal, then the keys are typed the way a user types them.
        terminal.tap()
        app.typeText("hello")
        let typed = app.staticTexts
            .matching(NSPredicate(format: "label CONTAINS %@", "hello")).firstMatch
        XCTAssertTrue(typed.waitForExistence(timeout: 60), "the typed text did not reach the guest")
        attachScreenshot("typed input")

        // The rotation changes the size the guest was told about.
        let size = app.staticTexts["guest-terminal-size"]
        XCUIDevice.shared.orientation = .landscapeLeft
        expectation(for: NSPredicate(format: "label != %@", portraitSize), evaluatedWith: size)
        waitForExpectations(timeout: 120)
        let landscapeSize = size.label
        XCTAssertNotEqual(landscapeSize, portraitSize,
                          "the rotation did not change the guest terminal size")
        attachScreenshot("landscape")

        // The size follows the orientation back.
        XCUIDevice.shared.orientation = .portrait
        expectation(for: NSPredicate(format: "label != %@", landscapeSize), evaluatedWith: size)
        waitForExpectations(timeout: 120)
        XCTAssertNotEqual(size.label, landscapeSize,
                          "the rotation back did not change the guest terminal size")
    }

    /// An interrupt reaches the guest program and ends it.
    func testAnInterruptEndsTheGuestProgram() {
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        let terminal = app.otherElements["guest-terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal is missing")
        rotate(to: .landscapeLeft)

        terminal.tap()
        terminal.typeKey("c", modifierFlags: .control)
        XCTAssertTrue(app.staticTexts["guest-exited"].waitForExistence(timeout: 60),
                      "the interrupt did not reach the guest program")
        attachScreenshot("the guest program ended")
    }

    /// The second launch verifies the root and reuses it instead of importing
    /// it again.
    func testTheSecondLaunchReusesTheVerifiedRoot() {
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        app.terminate()
        app.launchArguments = []
        app.launch()

        let state = app.staticTexts["guest-root-state"]
        XCTAssertTrue(state.waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")
        XCTAssertEqual(state.label, "reused verified root")
        attachScreenshot("second launch")
    }

    /// The size the guest terminal was told about, once it stopped changing.
    private func settledTerminalSize() -> String {
        let size = app.staticTexts["guest-terminal-size"]
        XCTAssertTrue(size.waitForExistence(timeout: 60), "the terminal size is not shown")
        var previous = size.label
        for _ in 0..<20 {
            usleep(500_000)
            let current = size.label
            if current == previous {
                return current
            }
            previous = current
        }
        return previous
    }

    /// Turns the device to the given orientation and waits for the host to
    /// report the new terminal size. The size after the turn is returned.
    private func rotate(to orientation: UIDeviceOrientation) -> String {
        let before = settledTerminalSize()
        XCUIDevice.shared.orientation = orientation
        let size = app.staticTexts["guest-terminal-size"]
        expectation(for: NSPredicate(format: "label != %@", before), evaluatedWith: size)
        waitForExpectations(timeout: 120)
        XCTAssertNotEqual(size.label, before, "the rotation did not change the guest terminal size")
        return settledTerminalSize()
    }

    /// Keeps a screenshot of the screen in the test result.
    private func attachScreenshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
