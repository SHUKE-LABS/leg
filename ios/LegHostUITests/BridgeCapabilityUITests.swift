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

    private func type(_ text: String) {
        app.typeText(text)
    }

    /// Waits for the terminal to show `text` as output.
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

    private func attachScreenshot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
