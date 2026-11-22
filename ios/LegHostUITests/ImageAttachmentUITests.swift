import XCTest

/// Images picked on the host and handed to a running guest.
///
/// The image sources are the DEBUG fixture: the simulator tests cannot drive
/// the system pickers. The handoff tests run a shell that plays the receiver,
/// so a test can stage a stale or live marker and show what the host wrote to
/// the terminal; the turn test runs the pinned `leg-tui` against the loopback
/// fixture.
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

    /// Every source stages an image with its preview. Without a live
    /// receiver the images stay; one is handed to a receiver as an opaque
    /// notice, the guest reads the same bytes, and the others are removed.
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

        // A marker no live receiver stands behind: nothing takes the handoff,
        // so the host retracts it with the marker and keeps the image.
        GuestKeyboard.focus(app)
        type(#": > "$LEG_HOST_ATTACHMENT_DIR/.receiver-v1"; echo STA''LE; cat -v"# + "\n")
        XCTAssertTrue(waitForOutput("STALE"), "the shell did not leave a marker")
        app.buttons["attachment-insert"].firstMatch.tap()
        XCTAssertTrue(waitForText("did not take the image"), "the untaken handoff is not reported")
        XCTAssertTrue(waitForState("attachments=3,handoffs=0"))
        app.buttons["attachment-insert"].firstMatch.tap()
        XCTAssertTrue(waitForText("cannot receive images"), "the stale marker still counts as a receiver")
        app.buttons["guest-key-interrupt"].tap()

        app.buttons["attachment-remove"].firstMatch.tap()
        app.buttons["attachment-remove"].firstMatch.tap()
        XCTAssertTrue(waitForState("attachments=1"))

        // The shell plays the receiver: it takes the file the notice names,
        // in its own directory, then shows what reached the terminal.
        GuestKeyboard.focus(app)
        type(#"D=$LEG_HOST_ATTACHMENT_DIR; : > "$D/.receiver-v1"; echo REA''DY; "#
            + #"until ls "$D" | grep -q ios-; do sleep 0.1; done; f=$(ls "$D" | grep ios-); "#
            + #"sha256sum "$D/$f" | cut -c1-16 | sed 's/^/SH''A=/'; rm "$D/$f"; cat -v"# + "\n")
        XCTAssertTrue(waitForOutput("READY"), "the shell did not announce the receiver")
        app.buttons["attachment-insert"].firstMatch.tap()
        XCTAssertTrue(waitForOutput("SHA=497790947d466676"), "the guest does not see the handed-over image")
        XCTAssertTrue(waitForState("attachments=0,handoffs=1"), "the image was not handed over")
        XCTAssertTrue(waitForOutput("leghost-attachment/v1"), "the notice did not reach the terminal")
        XCTAssertFalse(app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "Application Support"))
            .firstMatch.exists, "a host path reached the terminal")
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
        type(#"test -d "$LEG_HOST_ATTACHMENT_DIR" && echo COU''NT=$(ls -A "$LEG_HOST_ATTACHMENT_DIR" | wc -l | tr -d ' ')"# + "\n")
        XCTAssertTrue(waitForOutput("COUNT=0"), "a refused image reached the guest")
    }

    /// Text, an inserted image and text reach the provider as one turn.
    ///
    /// The pinned `leg-tui` receives the image into its draft, and the
    /// loopback fixture accepts the one request only when its last message is
    /// the text, an `image/png` block whose bytes are the fixture PNG, and
    /// the text, in that order. The terminal shows the image as its
    /// presentation only: no image bytes, no notice and no host path.
    func testAnInsertedImageReachesTheProviderAsOneOrderedTurn() {
        app.launchArguments = ["-leghost-fixture-reset", "-leghost-fixture", "image",
                               "-leghost-image-fixture", "png"] + Self.provider
        app.launch()
        XCTAssertTrue(app.otherElements["guest-terminal"].waitForExistence(timeout: startTimeout),
                      "the terminal is missing")
        GuestScreen.openComposer(app)

        GuestKeyboard.focus(app)
        type("IOS_IMAGE_BEFORE ")
        app.buttons["attach-photos"].tap()
        XCTAssertTrue(waitForState("attachments=1"), "the image was not staged")
        app.buttons["attachment-insert"].firstMatch.tap()
        XCTAssertTrue(waitForState("attachments=0,handoffs=1"), "leg-tui did not take the image")
        XCTAssertTrue(waitForScreen("IOS_IMAGE_BEFORE [image 1]"), "the image is not in the draft")
        GuestKeyboard.focus(app)
        type(" IOS_IMAGE_AFTER")
        app.buttons["guest-key-submit"].tap()

        XCTAssertTrue(waitForState("requests=1,statuses=200,validation=ok,"),
                      "the fixture did not accept one ordered text/image/text turn")
        XCTAssertTrue(waitForScreen("IOS_IMAGE_BEFORE [image: image/png 70 B] IOS_IMAGE_AFTER"),
                      "the transcript does not show the image presentation")
        XCTAssertTrue(waitForScreen("IOS_FIXTURE_FINAL_RESPONSE"), "the answer is not in the terminal")
        let screen = GuestScreen.text(app)
        for leak in ["iVBORw0KGgo", "IHDR", "leghost-attachment", "Application Support", "CoreSimulator"] {
            XCTAssertFalse(screen.contains(leak), "the terminal shows \(leak)")
        }
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

    /// Waits for the guest terminal to show the words of `text` in order.
    private func waitForScreen(_ text: String, timeout: TimeInterval = 120) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if GuestScreen.shows(text, in: GuestScreen.text(app)) { return true }
            usleep(3_000_000)
        }
        report("no screen text \"\(text)\"")
        return false
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
