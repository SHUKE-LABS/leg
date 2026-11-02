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

    /// The arguments every run of these tests uses.
    ///
    /// The host reads its provider configuration before it touches the guest
    /// root, so a run without a configuration stops at that boundary. The
    /// provider control gives the run one in memory, without the Keychain and
    /// without the fixture.
    private static let launchArguments = ["-leghost-provider", "anthropic"]

    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        // The reset makes the test start from a known state: the host removes
        // the provisioned root and imports it again.
        app.launchArguments = ["-leghost-reset"] + Self.launchArguments
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
        GuestKeyboard.focus(app)
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
    ///
    /// The interrupt is a control on the screen, so it works without a
    /// hardware keyboard.
    func testAnInterruptEndsTheGuestProgram() {
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        let terminal = app.otherElements["guest-terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal is missing")

        // The guest program is on the screen: it asks for a workspace.
        let prompt = app.staticTexts
            .matching(NSPredicate(format: "label CONTAINS %@", "workspace")).firstMatch
        XCTAssertTrue(prompt.waitForExistence(timeout: 60), "the guest program did not draw its screen")

        let interrupt = app.buttons["guest-key-interrupt"]
        XCTAssertTrue(interrupt.waitForExistence(timeout: 60), "the interrupt control is missing")
        interrupt.tap()

        XCTAssertTrue(app.staticTexts["guest-exited"].waitForExistence(timeout: 60),
                      "the interrupt did not reach the guest program")
        attachScreenshot("the guest program ended")
    }

    /// A helper process the guest spawns exits on its own, and the session
    /// keeps running.
    func testAChildExitDoesNotEndTheSession() {
        // The guest runs a shell, so the test can make it spawn a child that
        // exits while the session itself stays alive.
        app.terminate()
        app.launchArguments = Self.launchArguments + ["-leghost-reset", "-leghost-program", "/bin/sh"]
        app.launch()

        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        let terminal = app.otherElements["guest-terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal is missing")

        GuestKeyboard.focus(app)
        // The subshell is a child process: it exits on its own.
        app.typeText("(exit 0)\n")
        // The session is still alive, so the shell still runs commands. The
        // answer is not part of the command, so it cannot be the echo of it.
        app.typeText("echo $((6*7))\n")

        let answer = app.staticTexts
            .matching(NSPredicate(format: "label CONTAINS %@", "42")).firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 60),
                      "the session did not survive the exit of the child process")
        XCTAssertFalse(app.staticTexts["guest-exited"].exists,
                       "the exit of the child process ended the session")
        attachScreenshot("the session survived the child exit")
    }

    /// The second launch verifies the root and reuses it instead of importing
    /// it again.
    func testTheSecondLaunchReusesTheVerifiedRoot() {
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        app.terminate()
        app.launchArguments = Self.launchArguments
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

    /// Keeps a screenshot of the screen in the test result.
    private func attachScreenshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}

/// Drives the LegHost app against the loopback fixture.
///
/// The fixture serves the Anthropic Messages endpoint on the loopback
/// interface, so one guest turn runs without a provider. Every scenario has
/// its own failure class, and the fixture records what it saw: the number of
/// accepted requests, their statuses and the validation result.
final class LegHostFixtureUITests: XCTestCase {
    /// The host imports the guest root on the first launch, which takes a
    /// moment on a cold container.
    private let startTimeout: TimeInterval = 300
    /// The prompt the test sends.
    private let prompt = "IOS_FIXTURE_TURN"

    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        // Every test starts in portrait, so the terminal has one size.
        XCUIDevice.shared.orientation = .portrait
    }

    override func tearDownWithError() throws {
        app = nil
    }

    /// The guest asks for a workspace, sends one prompt and the fixture
    /// answers it: a text, one bash tool call and the final text.
    ///
    /// The fixture accepted exactly two requests, both with the status 200,
    /// and its validation passed: the first request asked for the tool call
    /// and the second one carried the tool result.
    func testTheSuccessScenarioCompletesOneTurn() {
        launch(["-leghost-fixture", "success", "-leghost-provider", "anthropic"])
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        driveOneTurn()

        XCTAssertTrue(waitForState("requests=2"), "the fixture did not accept two requests")
        let state = app.staticTexts["guest-fixture-state"]
        XCTAssertTrue(state.label.contains(
            "provision=1,boot=1,session=1,requests=2,statuses=200,200,validation=ok"),
            "the host state is \(state.label)")
        XCTAssertTrue(waitForTerminalText("IOS_FIXTURE_STREAM_TEXT"),
                      "the streamed text is not in the terminal")
        XCTAssertTrue(waitForTerminalText("IOS_FIXTURE_TOOL_RESULT"),
                      "the tool result is not in the terminal")
        XCTAssertTrue(waitForTerminalText("IOS_FIXTURE_FINAL_RESPONSE"),
                      "the final response is not in the terminal")
    }

    /// The provider answers the first request with a server error.
    ///
    /// The fixture recorded one request with the status 500, and the guest
    /// reported the provider failure in its terminal, not a transport failure
    /// and not a runtime failure.
    func testTheProviderErrorScenarioReportsAServerError() {
        launch(["-leghost-fixture", "provider-error", "-leghost-provider", "anthropic"])
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        driveOneTurn()

        XCTAssertTrue(waitForTerminalText(
            "provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR"),
            "the provider error is not in the terminal")
        XCTAssertTrue(waitForState("statuses=500"), "the fixture did not record the 500")
        XCTAssertTrue(waitForState("requests=1"), "the fixture did not record one request")
        XCTAssertFalse(terminalShows("transport error:"),
                       "the provider error was reported as a transport error")
        XCTAssertFalse(terminalShows("IOS_GUEST_RUNTIME_ERROR"),
                       "the provider error was reported as a runtime error")
    }

    /// The loopback port is closed, so the guest cannot reach the provider.
    ///
    /// The fixture accepted no request at all, and the guest reported a
    /// transport failure in its terminal.
    func testTheUnreachableScenarioReportsATransportError() {
        launch(["-leghost-fixture", "unreachable", "-leghost-provider", "anthropic"])
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        driveOneTurn()

        XCTAssertTrue(waitForTerminalText("transport error:"),
                      "the transport error is not in the terminal")
        XCTAssertTrue(waitForState("requests=0"), "the fixture accepted a request")
        XCTAssertFalse(terminalShows("provider server error"),
                       "the transport failure was reported as a provider error")
        XCTAssertFalse(terminalShows("IOS_GUEST_RUNTIME_ERROR"),
                       "the transport failure was reported as a runtime error")
    }

    /// The guest program is not there, so the guest runtime cannot start it.
    ///
    /// The host reports the runtime diagnostic and starts no session, so the
    /// fixture never saw a request. The guest program never ran, so the
    /// terminal is the only place where the run can show what went wrong:
    /// the diagnostic is written into the terminal, and the transcript of the
    /// run holds it as well.
    func testTheRuntimeFailureScenarioReportsTheRuntimeDiagnostic() {
        launch(["-leghost-fixture", "success", "-leghost-provider", "anthropic",
                "-leghost-runtime-failure"])

        XCTAssertTrue(waitForTerminalText("IOS_GUEST_RUNTIME_ERROR", timeout: startTimeout),
                      "the runtime diagnostic is not in the terminal")
        XCTAssertTrue(waitForStatus("IOS_GUEST_RUNTIME_ERROR", timeout: startTimeout),
                      "the runtime diagnostic is not in the status of the host")
        XCTAssertTrue(waitForState("session=0"), "the host started a session")
        XCTAssertTrue(waitForState("requests=0"), "the fixture accepted a request")
        XCTAssertFalse(terminalShows("provider server error"),
                       "the runtime failure was reported as a provider error")
        XCTAssertFalse(terminalShows("transport error:"),
                       "the runtime failure was reported as a transport error")
    }

    /// Without a provider configuration the host stops before it provisions
    /// the guest.
    func testAMissingConfigurationIsReportedBeforeTheGuestIsProvisioned() {
        launch(["-leghost-fixture", "success", "-leghost-no-provider"])

        XCTAssertTrue(waitForStatus("IOS_PROVIDER_CONFIG_MISSING", timeout: startTimeout),
                      "the missing configuration diagnostic is not in the status of the host")
        XCTAssertTrue(waitForState("provision=0,boot=0,session=0"),
                      "the host provisioned the guest")
        XCTAssertTrue(waitForState("requests=0"), "the fixture accepted a request")
        XCTAssertTrue(waitForState("listener=0"), "the host started the fixture")
    }

    /// The two OpenAI wire variants are refused at the same boundary as a
    /// missing configuration.
    func testTheOpenAIProvidersAreRefusedBeforeTheGuestIsProvisioned() {
        for provider in ["openai-chat-completions", "openai-responses"] {
            app.terminate()
            launch(["-leghost-fixture", "success", "-leghost-provider", provider])

            XCTAssertTrue(waitForStatus("IOS_PROVIDER_CONFIG_UNSUPPORTED", timeout: startTimeout),
                          "the unsupported provider diagnostic is not in the status of the host for \(provider)")
            XCTAssertTrue(waitForState("provision=0,boot=0,session=0"),
                          "the host provisioned the guest for \(provider)")
            XCTAssertTrue(waitForState("requests=0"), "the fixture accepted a request for \(provider)")
            XCTAssertTrue(waitForState("listener=0"), "the host started the fixture for \(provider)")
        }
    }

    // MARK: - Helpers

    /// Starts the app with the given launch arguments.
    private func launch(_ arguments: [String]) {
        app.launchArguments = arguments
        app.launch()
    }

    /// Drives one turn in the guest program.
    ///
    /// The guest program asks for a workspace, shows the first-run warning
    /// and then offers the composer. The test fills in the workspace,
    /// acknowledges the warning and sends the prompt with the send control.
    private func driveOneTurn() {
        let terminal = app.otherElements["guest-terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal is missing")

        XCTAssertTrue(waitForTerminalText("Enter a path and press Enter to select it."),
                      "the guest program did not ask for a workspace")
        focusGuest()
        app.typeText("/tmp")
        app.typeText("\n")

        // The first-run warning comes next, unless a previous run already
        // acknowledged it.
        if waitForTerminalText("Press Enter to acknowledge and open the composer.", timeout: 30) {
            focusGuest()
            app.typeText("\n")
        }

        XCTAssertTrue(waitForTerminalText("catalog: "), "the composer did not appear")
        focusGuest()
        app.typeText(prompt)
        let submit = app.buttons["guest-key-submit"]
        XCTAssertTrue(submit.waitForExistence(timeout: 30), "the send control is missing")
        submit.tap()
    }

    /// Gives the keyboard to the guest.
    ///
    /// The keyboard goes into a hidden text field under the terminal, so the
    /// terminal has to be tapped to give the field the focus. The tap can be
    /// swallowed by the terminal page, so it is repeated until the tree shows
    /// the field holding the focus.
    private func focusGuest() {
        GuestKeyboard.focus(app)
    }

    /// Waits for the host state to hold `text`.
    private func waitForState(_ text: String, timeout: TimeInterval = 120) -> Bool {
        let state = app.staticTexts["guest-fixture-state"]
        guard state.waitForExistence(timeout: timeout) else { return false }
        let expectation = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS %@", text),
            object: state)
        return XCTWaiter().wait(for: [expectation], timeout: timeout) == .completed
    }

    /// Waits for the terminal to show `text`.
    ///
    /// The terminal is the screen of the guest program, so the text of a turn
    /// is read from there and not from the screen of the host. The terminal
    /// draws a line as several elements, so a phrase can be split over them
    /// and the gap between two words can hold more than one space. The
    /// terminal is read as one string and the words are looked for in order,
    /// so a phrase does not have to sit in one element.
    private func waitForTerminalText(_ text: String, timeout: TimeInterval = 120) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if Self.shows(text, in: terminalText()) { return true }
            // The screen is read through the accessibility tree, and the tree
            // of a simulator is read one snapshot at a time. The wait is
            // polled every other second, so a long wait does not keep the
            // accessibility server of the simulator busy.
            usleep(2_000_000)
        }
        return false
    }

    /// Waits for the status of the host to show `text`.
    ///
    /// The status of the host holds the diagnostics the host produces itself:
    /// the configuration of the provider and the runtime of the guest.
    private func waitForStatus(_ text: String, timeout: TimeInterval = 120) -> Bool {
        let detail = app.staticTexts["guest-status-detail"]
        guard detail.waitForExistence(timeout: timeout) else { return false }
        let expectation = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS %@", text),
            object: detail)
        return XCTWaiter().wait(for: [expectation], timeout: timeout) == .completed
    }

    /// The text of the guest terminal.
    ///
    /// The terminal is read as one snapshot of its own tree. The guest
    /// program draws while the terminal is read, so the elements of the tree
    /// are resolved one by one against a tree that changes under the reader,
    /// and the read fails on a screen that moves. The snapshot of the
    /// terminal holds the labels of the terminal as one string.
    private func terminalText() -> String {
        app.otherElements["guest-terminal"].debugDescription
    }

    /// Whether the terminal shows `text` right now.
    private func terminalShows(_ text: String) -> Bool {
        Self.shows(text, in: terminalText())
    }

    /// Whether `text` shows up in `screen`.
    ///
    /// The words of `text` have to show up in order, but not next to each
    /// other: the terminal draws the words of a line as they are written.
    private static func shows(_ text: String, in screen: String) -> Bool {
        var rest = screen[...]
        for word in text.split(separator: " ", omittingEmptySubsequences: true) {
            guard let range = rest.range(of: word) else { return false }
            rest = rest[range.upperBound...]
        }
        return true
    }
}

/// The keyboard of the host, which forwards it to the guest program.
///
/// The keyboard goes into a hidden text field under the terminal, so the
/// terminal has to be tapped to give the field the focus. The tap can be
/// swallowed by the terminal page, and a simulator under load swallows it
/// more often, so the tap is repeated until the tree shows the field holding
/// the focus.
enum GuestKeyboard {
    /// Gives the keyboard to the guest program.
    static func focus(_ app: XCUIApplication) {
        let terminal = app.otherElements["guest-terminal"]
        let top = terminal.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.05))
        for _ in 0..<6 {
            if hasFocus(app) { return }
            top.tap()
            usleep(700_000)
        }
    }

    /// Whether the hidden input field holds the keyboard focus.
    ///
    /// The accessibility tree marks the element that holds the focus, so the
    /// field is looked for in it.
    private static func hasFocus(_ app: XCUIApplication) -> Bool {
        app.debugDescription
            .split(separator: "\n")
            .contains { $0.contains("guest-input") && $0.contains("Focused") }
    }
}
