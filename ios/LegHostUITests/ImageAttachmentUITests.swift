import XCTest

/// Images picked on the host and handed to a running guest.
///
/// The image sources are the DEBUG fixture: the simulator tests cannot drive
/// the system pickers. The guest is a shell, which plays the receiver: the
/// pinned guest bundle predates the `leg-tui` receiver, so the shell
/// announces one and shows what the host wrote to the terminal.
final class ImageAttachmentUITests: XCTestCase {
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

    /// Every source stages an image with its preview; one is handed to the
    /// guest as an opaque notice, the guest reads the same bytes, and the
    /// others are removed.
    func testImagesFromEverySourceReachTheGuestAsOpaqueNotices() {
        launchShell(["-leghost-reset", "-leghost-image-fixture", "png"])
        XCTAssertTrue(waitForState("attachments=0"))

        for (button, label) in [("attach-photos", "Photos · fixture-photos.png"),
                                ("attach-files", "Files · fixture-files.png"),
                                ("attach-pasteboard", "Clipboard · fixture-pasteboard.png")] {
            app.buttons[button].tap()
            XCTAssertTrue(waitForText(label), "no preview for \(button)")
        }
        XCTAssertTrue(waitForState("attachments=3"))
        XCTAssertTrue(waitForText("image/png · 70 bytes"), "the preview has no type or size")

        // Without a receiver in the guest the image stays on the host.
        app.buttons["attachment-insert"].firstMatch.tap()
        XCTAssertTrue(waitForText("cannot receive images"), "the missing receiver is not reported")
        XCTAssertTrue(waitForState("attachments=3"))

        app.buttons["attachment-remove"].firstMatch.tap()
        app.buttons["attachment-remove"].firstMatch.tap()
        XCTAssertTrue(waitForState("attachments=1"))

        // The shell announces a receiver and echoes what reaches the terminal.
        GuestKeyboard.focus(app)
        type(#": > "$LEG_HOST_ATTACHMENT_DIR/.receiver-v1"; echo REA''DY; cat -v"# + "\n")
        XCTAssertTrue(waitForOutput("READY"), "the shell did not announce the receiver")
        app.buttons["attachment-insert"].firstMatch.tap()
        XCTAssertTrue(waitForState("handoffs=1"), "the image was not handed over")
        XCTAssertTrue(waitForState("attachments=0"))
        XCTAssertTrue(waitForOutput("leghost-attachment/v1"), "the notice did not reach the terminal")
        XCTAssertFalse(app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "Application Support"))
            .firstMatch.exists, "a host path reached the terminal")

        // The guest reads the bytes the notice names, in its own directory.
        app.buttons["guest-key-interrupt"].tap()
        GuestKeyboard.focus(app)
        type(#"sha256sum "$LEG_HOST_ATTACHMENT_DIR"/ios-* | cut -c1-16 | sed 's/^/SH''A=/'"# + "\n")
        XCTAssertTrue(waitForOutput("SHA=497790947d466676"), "the guest does not see the handed-over image")
    }

    /// An image whose bytes do not match its type is refused with an
    /// actionable status, and nothing reaches the guest.
    func testARejectedImageStagesNothing() {
        launchShell(["-leghost-reset", "-leghost-image-fixture", "mismatch"])
        XCTAssertTrue(waitForState("attachments=0"))

        app.buttons["attach-files"].tap()
        XCTAssertTrue(waitForText("does not match its declared type"), "the mismatch is not reported")
        XCTAssertTrue(waitForState("attachments=0"))
        XCTAssertFalse(app.buttons["attachment-insert"].exists)

        GuestKeyboard.focus(app)
        type(#"echo COU''NT=$(ls -A "$LEG_HOST_ATTACHMENT_DIR" | wc -l | tr -d ' ')"# + "\n")
        XCTAssertTrue(waitForOutput("COUNT=0"), "a refused image reached the guest")
    }

    // MARK: Helpers

    private func launchShell(_ arguments: [String]) {
        app.launchArguments = arguments + Self.provider + ["-leghost-program", "/bin/sh"]
        app.launch()
        XCTAssertTrue(app.otherElements["guest-terminal"].waitForExistence(timeout: startTimeout),
                      "the terminal is missing")
        GuestKeyboard.focus(app)
    }

    private func type(_ text: String) {
        app.typeText(text)
    }

    private func waitForText(_ text: String, timeout: TimeInterval = 30) -> Bool {
        let found = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
            .waitForExistence(timeout: timeout)
        if !found {
            report("no text \"\(text)\"")
        }
        return found
    }

    private func waitForOutput(_ text: String, timeout: TimeInterval = 60) -> Bool {
        waitForText(text, timeout: timeout)
    }

    private func waitForState(_ text: String, timeout: TimeInterval = 120) -> Bool {
        let state = app.staticTexts["guest-fixture-state"]
        guard state.waitForExistence(timeout: timeout) else {
            report("no state")
            return false
        }
        let expectation = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS %@", text), object: state)
        guard XCTWaiter().wait(for: [expectation], timeout: timeout) == .completed else {
            report("no state \"\(text)\"")
            return false
        }
        return true
    }

    private func report(_ failure: String) {
        let running = app.state == .runningForeground || app.state == .runningBackground
        print("[ImageAttachmentUITests] \(failure); app state \(app.state.rawValue)"
            + (running ? "\n" + app.debugDescription : "; the app is not running"))
    }
}
