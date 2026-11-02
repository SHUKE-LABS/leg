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
        app.launchArguments = ["-leghost-reset", "-leghost-program", "/bin/sh"]
        app.launch()

        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        let terminal = app.otherElements["guest-terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 60), "the terminal is missing")

        terminal.tap()
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
        XCTAssertTrue(waitForTerminal("IOS_FIXTURE_STREAM_TEXT"),
                      "the streamed text is not on the screen")
        XCTAssertTrue(waitForTerminal("IOS_FIXTURE_TOOL_RESULT"),
                      "the tool result is not on the screen")
        XCTAssertTrue(waitForTerminal("IOS_FIXTURE_FINAL_RESPONSE"),
                      "the final response is not on the screen")
    }

    /// The provider answers the first request with a server error.
    ///
    /// The fixture recorded one request with the status 500, and the guest
    /// reported the provider failure, not a transport failure and not a
    /// runtime failure.
    func testTheProviderErrorScenarioReportsAServerError() {
        launch(["-leghost-fixture", "provider-error", "-leghost-provider", "anthropic"])
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        driveOneTurn()

        XCTAssertTrue(waitForTerminal(
            "provider server error (500, api_error): IOS_FIXTURE_PROVIDER_ERROR"),
            "the provider error is not on the screen")
        XCTAssertTrue(waitForState("statuses=500"), "the fixture did not record the 500")
        XCTAssertTrue(waitForState("requests=1"), "the fixture did not record one request")
        XCTAssertFalse(terminalContains("transport error:"),
                       "the provider error was reported as a transport error")
        XCTAssertFalse(terminalContains("IOS_GUEST_RUNTIME_ERROR"),
                       "the provider error was reported as a runtime error")
    }

    /// The loopback port is closed, so the guest cannot reach the provider.
    ///
    /// The fixture accepted no request at all, and the guest reported a
    /// transport failure.
    func testTheUnreachableScenarioReportsATransportError() {
        launch(["-leghost-fixture", "unreachable", "-leghost-provider", "anthropic"])
        XCTAssertTrue(app.staticTexts["guest-root-state"].waitForExistence(timeout: startTimeout),
                      "the host did not reach the running state")

        driveOneTurn()

        XCTAssertTrue(waitForTerminal("transport error:"),
                      "the transport error is not on the screen")
        XCTAssertTrue(waitForState("requests=0"), "the fixture accepted a request")
        XCTAssertFalse(terminalContains("provider server error"),
                       "the transport failure was reported as a provider error")
        XCTAssertFalse(terminalContains("IOS_GUEST_RUNTIME_ERROR"),
                       "the transport failure was reported as a runtime error")
    }

    /// The guest program is not there, so the guest runtime cannot start it.
    ///
    /// The host reports the runtime diagnostic and starts no session, so the
    /// fixture never saw a request.
    func testTheRuntimeFailureScenarioReportsTheRuntimeDiagnostic() {
        launch(["-leghost-fixture", "success", "-leghost-provider", "anthropic",
                "-leghost-runtime-failure"])

        XCTAssertTrue(waitForTerminal("IOS_GUEST_RUNTIME_ERROR", timeout: startTimeout),
                      "the runtime diagnostic is not on the screen")
        XCTAssertTrue(waitForState("session=0"), "the host started a session")
        XCTAssertTrue(waitForState("requests=0"), "the fixture accepted a request")
        XCTAssertFalse(terminalContains("provider server error"),
                       "the runtime failure was reported as a provider error")
        XCTAssertFalse(terminalContains("transport error:"),
                       "the runtime failure was reported as a transport error")
    }

    /// Without a provider configuration the host stops before it provisions
    /// the guest.
    func testAMissingConfigurationIsReportedBeforeTheGuestIsProvisioned() {
        launch(["-leghost-fixture", "success", "-leghost-no-provider"])

        XCTAssertTrue(waitForTerminal("IOS_PROVIDER_CONFIG_MISSING", timeout: startTimeout),
                      "the missing configuration diagnostic is not on the screen")
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

            XCTAssertTrue(waitForTerminal("IOS_PROVIDER_CONFIG_UNSUPPORTED", timeout: startTimeout),
                          "the unsupported provider diagnostic is not on the screen for \(provider)")
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

        XCTAssertTrue(waitForTerminal("Enter a path and press Enter to select it."),
                      "the guest program did not ask for a workspace")
        terminal.tap()
        app.typeText("/tmp")
        app.typeText("\n")

        // The first-run warning comes next, unless a previous run already
        // acknowledged it.
        if waitForTerminal("Press Enter to acknowledge and open the composer.", timeout: 30) {
            app.typeText("\n")
        }

        XCTAssertTrue(waitForTerminal("catalog: "), "the composer did not appear")
        app.typeText(prompt)
        let submit = app.buttons["guest-key-submit"]
        XCTAssertTrue(submit.waitForExistence(timeout: 30), "the send control is missing")
        submit.tap()
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
    private func waitForTerminal(_ text: String, timeout: TimeInterval = 120) -> Bool {
        app.staticTexts
            .matching(NSPredicate(format: "label CONTAINS %@", text))
            .firstMatch
            .waitForExistence(timeout: timeout)
    }

    /// Whether the terminal shows `text` right now.
    private func terminalContains(_ text: String) -> Bool {
        app.staticTexts
            .matching(NSPredicate(format: "label CONTAINS %@", text))
            .firstMatch
            .exists
    }
}
