import XCTest

/// The capability bridge between the guest and the host, in a running guest.
///
/// The pinned guest bundle carries a `leg-bridge` that predates the Photos
/// and Files capabilities, so the exchange uses `files.list`, which the
/// listener does not serve: the reply proves that a real guest `connect()`
/// reached the listener of this generation and that the listener answered.
final class BridgeCapabilityUITests: XCTestCase {
    private let startTimeout: TimeInterval = 300
    private static let provider = ["-leghost-provider", "anthropic"]
    private static let request = #"printf '%s\n' '{"version":1,"request_id":"r-1","capability":"files.list","input":{}}' | /usr/local/bin/leg-bridge"#
    /// Reduces an answer of the bridge to `REASON=<reason>`; the echo of the
    /// command line does not contain the marker.
    private static let reasonFilter = #" | sed -n 's/.*"reason":"\([a-z_]*\)".*/REAS''ON=\1/p'"#

    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
    }

    override func tearDownWithError() throws {
        app = nil
    }

    /// The guest finds the endpoint, connects to it, and gets the host's
    /// answer.
    func testTheGuestReachesTheListener() {
        launchShell(["-leghost-reset"])
        XCTAssertTrue(waitForState("bridge=listening"), "the listener is not up")

        // The endpoint is the fake socket inode the host made in the guest.
        type("ls -l /run/ | grep leghost-bridge\n")
        XCTAssertTrue(waitForOutput("srw"), "the guest has no socket at the endpoint")

        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("capability_unsupported"), "the host did not answer the guest")
        XCTAssertTrue(waitForState("replies=1"), "the listener did not count its reply")
        attachScreenshot("bridge reply")
    }

    /// Without a listener the guest gets no endpoint and no socket variable.
    func testWithoutAListenerTheGuestHasNoTransport() {
        launchShell(["-leghost-reset", "-leghost-bridge-off"])
        XCTAssertTrue(waitForState("bridge=off"))

        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("no_host_transport"), "the guest found a transport")
        XCTAssertTrue(waitForState("replies=0"))
    }

    /// A listener that cannot start fails the session before the guest
    /// program starts, and maps no guest endpoint.
    func testAListenerThatCannotStartStartsNoGuest() {
        app.launchArguments = ["-leghost-reset", "-leghost-bridge-listener-failure"] + Self.provider
            + ["-leghost-program", "/bin/sh"]
        app.launch()
        XCTAssertTrue(waitForStatus("IOS_BRIDGE_LISTENER_ERROR"), "the listener failure is not reported")
        XCTAssertTrue(waitForState("session=0"), "the guest started without a listener")
        XCTAssertTrue(waitForState("bridge=off"))
        XCTAssertTrue(waitForState("endpoint=none"))
    }

    /// The real guest bridge refuses what it cannot send before it connects,
    /// and uses a request id once: the duplicate opens no host connection.
    func testTheGuestRefusesBadRequestsWithoutReachingTheHost() {
        launchShell(["-leghost-reset"])
        XCTAssertTrue(waitForState("bridge=listening"))
        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("capability_unsupported"), "the host did not answer the guest")
        XCTAssertTrue(waitForState("replies=1,"))

        // Each refusal prints its reason alone on a line, so the check does
        // not depend on where the terminal wraps the answer.
        type(Self.request + Self.reasonFilter + "\n")
        XCTAssertTrue(waitForOutput("REASON=duplicate_request_id"), "the guest sent a request id twice")
        type(#"printf 'not json\n' | /usr/local/bin/leg-bridge"# + Self.reasonFilter + "\n")
        XCTAssertTrue(waitForOutput("REASON=malformed_request"))
        type(#"printf '%s\n' '{"version":2,"request_id":"r-2","capability":"files.list","input":{}}' | /usr/local/bin/leg-bridge"#
            + Self.reasonFilter + "\n")
        XCTAssertTrue(waitForOutput("REASON=unsupported_version"))
        type("/usr/local/bin/leg-bridge-missing </dev/null; echo BRIDGE''_EXIT=$?\n")
        XCTAssertTrue(waitForOutput("BRIDGE_EXIT=127"), "a missing bridge did not fail as a missing command")
        XCTAssertTrue(waitForState("replies=1,"), "a refused request reached the host")
    }

    /// The background stops the session and closes its endpoint; the retry
    /// opens a fresh one.
    func testTheBackgroundClosesTheEndpointAndTheRetryOpensAFreshOne() {
        launchShell(["-leghost-reset"])
        XCTAssertTrue(waitForState("bridge=listening"))
        XCTAssertTrue(waitForState("sockets=1,"))

        // The app needs a moment to leave the foreground before it is
        // activated again.
        XCUIDevice.shared.press(.home)
        sleep(5)
        app.activate()
        XCTAssertTrue(app.staticTexts["guest-interrupted"].waitForExistence(timeout: startTimeout),
                      "the session was not interrupted")
        XCTAssertTrue(waitForState("bridge=closed"))
        XCTAssertTrue(waitForState("sockets=0,"), "the interrupted session left its socket")
        XCTAssertTrue(waitForState("endpoint=none"), "the interrupted session left its guest endpoint")

        XCTAssertTrue(retryUntilSession(2), "the retry did not start a new session")
        XCTAssertTrue(waitForState("bridge=listening"))
        XCTAssertTrue(waitForState("sockets=1,"))
        GuestKeyboard.focus(app)
        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("capability_unsupported"), "the retry did not reach its listener")
    }

    /// A host that is killed cannot tear its listener down; the next launch
    /// still ends up with exactly its own fresh endpoint.
    func testARelaunchAfterAKillLeavesOnlyTheFreshEndpoint() {
        launchShell(["-leghost-reset"])
        XCTAssertTrue(waitForState("bridge=listening"))
        XCTAssertTrue(waitForState("sockets=1,"))

        app.terminate()
        launchShell([])
        XCTAssertTrue(waitForState("bridge=listening"))
        XCTAssertTrue(waitForState("sockets=1,"), "the endpoint of the killed host is still there")
        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("capability_unsupported"), "the relaunch did not reach its listener")
    }

    /// A start that fails closes the listener it opened.
    func testAFailedStartClosesTheListener() {
        app.launchArguments = ["-leghost-reset", "-leghost-runtime-failure"] + Self.provider
        app.launch()
        XCTAssertTrue(waitForState("bridge=closed", timeout: startTimeout), "the listener outlived the failed start")
        XCTAssertTrue(waitForState("session=0"))

        // The retry runs a new generation from nothing.
        app.terminate()
        launchShell([])
        XCTAssertTrue(waitForState("bridge=listening"))
        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("capability_unsupported"), "the retry did not reach its listener")
    }

    /// A start that fails after the runtime made the endpoint removes it: a
    /// later session without a listener finds no endpoint.
    func testAStartThatFailsAfterTheEndpointWasMadeRemovesIt() {
        failTheStart(faults: "hold")
        XCTAssertTrue(waitForState("endpoint=none"), "the hook kept a record of a removed endpoint")
        assertTheEndpointIs(gone: true)
    }

    /// A socket that takes the endpoint before the runtime makes its own
    /// fails the start and is not removed by its cleanup.
    func testAStartLosingTheEndpointToAnotherSocketLeavesThatSocket() {
        failTheStart(faults: "race")
        XCTAssertTrue(waitForState("endpoint=none"), "the hook recorded an endpoint it did not make")
        assertTheEndpointIs(gone: false)
    }

    /// A socket that replaces the endpoint the runtime made is not removed by
    /// the cleanup of the failed start.
    func testAStartWhoseEndpointWasReplacedLeavesTheReplacement() {
        failTheStart(faults: "replace")
        XCTAssertTrue(waitForState("endpoint=none"), "the hook kept a record of a replaced endpoint")
        assertTheEndpointIs(gone: false)
    }

    /// A cleanup that cannot look the endpoint up keeps the record of it, and
    /// the next start replaces the socket that was left.
    func testACleanupThatCannotLookUpTheEndpointKeepsItsRecord() {
        failTheStart(faults: "hold,cleanup-lookup")
        XCTAssertTrue(waitForState("endpoint=kept"), "the hook dropped the record of an endpoint it left")

        app.terminate()
        launchShell([])
        XCTAssertTrue(waitForState("bridge=listening"))
        type(Self.request + "\n")
        XCTAssertTrue(waitForOutput("capability_unsupported"), "the next start did not replace the endpoint")
    }

    /// A cleanup that failed keeps its hold with its record, so a socket that
    /// replaces the endpoint afterwards cannot take the recorded inode number
    /// and survives the retry.
    func testACleanupRetryLeavesASocketThatReplacedTheEndpoint() {
        failTheStart(faults: "hold,cleanup-lookup,cleanup-retry")
        XCTAssertTrue(waitForState("endpoint=none"), "the retry kept the record of a replaced endpoint")
        assertTheEndpointIs(gone: false)
    }

    // MARK: Helpers

    /// Starts a shell whose start fails with the endpoint `faults`.
    private func failTheStart(faults: String) {
        app.launchArguments = ["-leghost-reset", "-leghost-bridge-endpoint-failure", faults] + Self.provider
            + ["-leghost-program", "/bin/sh"]
        app.launch()
        XCTAssertTrue(waitForState("bridge=closed", timeout: startTimeout), "the start did not fail")
        XCTAssertTrue(waitForState("session=0"))
    }

    /// Relaunches without a listener and checks whether the guest has a
    /// socket at the endpoint.
    private func assertTheEndpointIs(gone: Bool) {
        app.terminate()
        launchShell(["-leghost-bridge-off"])
        XCTAssertTrue(waitForState("bridge=off"))
        if gone {
            type("test -e /run/leghost-bridge.sock || echo ENDPOINT''_GONE\n")
            XCTAssertTrue(waitForOutput("ENDPOINT_GONE"), "the failed start left its endpoint in the guest")
        } else {
            type("test -S /run/leghost-bridge.sock && echo ENDPOINT''_KEPT\n")
            XCTAssertTrue(waitForOutput("ENDPOINT_KEPT"), "the failed start removed a socket it did not make")
        }
    }

    private func launchShell(_ arguments: [String]) {
        app.launchArguments = arguments + Self.provider + ["-leghost-program", "/bin/sh"]
        app.launch()
        XCTAssertTrue(app.otherElements["guest-terminal"].waitForExistence(timeout: startTimeout),
                      "the terminal is missing")
        GuestKeyboard.focus(app)
    }

    /// Taps the retry until session `number` runs. A loaded simulator drops
    /// an activation now and then; the app falls back to the background and
    /// interrupts the retry, and shows the retry control again.
    private func retryUntilSession(_ number: Int) -> Bool {
        for _ in 0..<3 {
            let retry = app.buttons["guest-retry"]
            guard retry.waitForExistence(timeout: 60) else { break }
            if app.state != .runningForeground {
                app.activate()
            }
            _ = app.wait(for: .runningForeground, timeout: 30)
            retry.tap()
            if waitForState("session=\(number)", timeout: 150) {
                return true
            }
        }
        return false
    }

    private func type(_ text: String) {
        app.typeText(text)
    }

    /// Waits for the terminal to show `text` as output.
    private func waitForOutput(_ text: String, timeout: TimeInterval = 60) -> Bool {
        let found = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
            .waitForExistence(timeout: timeout)
        if !found {
            report("no output \"\(text)\"")
        }
        return found
    }

    /// Prints the element tree, with the state, the status and the terminal
    /// text, to the log of the run. One snapshot, so a screen that changes
    /// meanwhile cannot fail the report.
    private func report(_ failure: String) {
        let running = app.state == .runningForeground || app.state == .runningBackground
        print("[BridgeCapabilityUITests] \(failure); app state \(app.state.rawValue)"
            + (running ? "\n" + app.debugDescription : "; the app is not running"))
    }

    private func waitForStatus(_ text: String, timeout: TimeInterval = 300) -> Bool {
        let detail = app.staticTexts["guest-status-detail"]
        guard detail.waitForExistence(timeout: timeout) else { return false }
        let expectation = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS %@", text), object: detail)
        return XCTWaiter().wait(for: [expectation], timeout: timeout) == .completed
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

    private func attachScreenshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
