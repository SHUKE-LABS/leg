import XCTest
@testable import LegHost

/// The lifecycle of the guest session.
///
/// The test drives the machine with a host it controls, so the gate of the
/// lifecycle is checked without a guest: two starts must not run two chains,
/// an interruption must release the session exactly once, and a retry must
/// run a new chain.
@MainActor
final class GuestHostLifecycleTests: XCTestCase {
    /// The host the test drives the lifecycle with.
    ///
    /// The host counts the operations it is asked for and can hold the chain
    /// at a gate, so the test decides when the chain goes on.
    private final class FakeHost: GuestHostLifecycleDelegate {
        var prepares = 0
        var provisions = 0
        var boots = 0
        var sessions = 0
        var teardowns = 0
        var persistedTrails = 0
        /// Called when the session started.
        var onSessionStarted: (() -> Void)?
        /// The gate the chain waits at, when the test set one.
        var gate: Gate?
        /// Whether the first operation fails.
        var fails = false

        func prepareHost() async throws {
            prepares += 1
            if fails {
                throw GuestHostError(message: "the host could not start")
            }
            await gate?.wait()
        }

        func provisionGuest() async throws {
            provisions += 1
            await gate?.wait()
        }

        func bootGuest() async throws {
            boots += 1
            await gate?.wait()
        }

        func startGuestSession() async throws {
            sessions += 1
            await gate?.wait()
            // The callback fires once, so a second chain does not fulfil the
            // same expectation twice.
            let callback = onSessionStarted
            onSessionStarted = nil
            callback?()
        }

        func teardownGuest() async {
            teardowns += 1
        }

        func persistGuestTrail() async {
            persistedTrails += 1
        }
    }

    /// The gate the chain of the test waits at.
    ///
    /// The gate holds the chain until the test opens it, so the test can
    /// interrupt a chain that is still running. A cancelled chain lets the
    /// gate go as well, so the chain can unwind instead of hanging in the
    /// gate while the interruption waits for it.
    private final class Gate {
        private let lock = NSLock()
        private var waiters: [CheckedContinuation<Void, Never>] = []
        private var isOpen = false
        private var isCancelled = false

        /// Lets every waiter go on.
        func open() {
            lock.lock()
            isOpen = true
            let waiting = waiters
            waiters = []
            lock.unlock()
            for waiter in waiting {
                waiter.resume()
            }
        }

        /// Waits until the gate is open.
        func wait() async {
            await withTaskCancellationHandler {
                await withCheckedContinuation { continuation in
                    lock.lock()
                    if isOpen || isCancelled {
                        lock.unlock()
                        continuation.resume()
                        return
                    }
                    waiters.append(continuation)
                    lock.unlock()
                }
            } onCancel: {
                self.lock.lock()
                self.isCancelled = true
                let waiting = self.waiters
                self.waiters = []
                self.lock.unlock()
                for waiter in waiting {
                    waiter.resume()
                }
            }
        }
    }

    /// Two starts run one chain.
    func testTwoStartsRunOneChain() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host
        let started = expectation(description: "the session started")
        host.onSessionStarted = { started.fulfill() }

        lifecycle.start()
        lifecycle.start()

        await fulfillment(of: [started], timeout: 5)
        // A second chain would have time to start here.
        try await Task.sleep(nanoseconds: 200_000_000)

        XCTAssertEqual(host.prepares, 1)
        XCTAssertEqual(host.provisions, 1)
        XCTAssertEqual(host.boots, 1)
        XCTAssertEqual(host.sessions, 1)
        XCTAssertEqual(lifecycle.state, .running)
    }

    /// A start after the session is up does not run a second chain.
    func testAStartAfterTheSessionIsUpIsRefused() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host
        let started = expectation(description: "the session started")
        host.onSessionStarted = { started.fulfill() }

        lifecycle.start()
        await fulfillment(of: [started], timeout: 5)
        lifecycle.start()
        try await Task.sleep(nanoseconds: 200_000_000)

        XCTAssertEqual(host.sessions, 1)
        XCTAssertEqual(lifecycle.state, .running)
    }

    /// An interruption releases the session once and writes the trail out.
    func testAnInterruptionReleasesTheSessionOnce() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host
        let started = expectation(description: "the session started")
        host.onSessionStarted = { started.fulfill() }

        lifecycle.start()
        await fulfillment(of: [started], timeout: 5)

        await lifecycle.interrupt()
        XCTAssertEqual(host.teardowns, 1)
        XCTAssertEqual(host.persistedTrails, 1)
        XCTAssertEqual(lifecycle.state, .interrupted)

        // A second interruption releases nothing a second time.
        await lifecycle.interrupt()
        XCTAssertEqual(host.teardowns, 1)
        XCTAssertEqual(host.persistedTrails, 1)
    }

    /// An interruption of a chain that is still running releases the session
    /// once, and the chain does not go on.
    func testAnInterruptionOfARunningChainStopsTheChain() async throws {
        let host = FakeHost()
        let gate = Gate()
        host.gate = gate
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host

        lifecycle.start()
        // The chain waits in the first operation.
        try await Task.sleep(nanoseconds: 100_000_000)
        XCTAssertEqual(host.prepares, 1)
        XCTAssertEqual(host.provisions, 0)

        await lifecycle.interrupt()
        gate.open()
        try await Task.sleep(nanoseconds: 200_000_000)

        XCTAssertEqual(host.teardowns, 1)
        XCTAssertEqual(host.persistedTrails, 1)
        XCTAssertEqual(lifecycle.state, .interrupted)
        // The chain did not go on: the operations after the gate never ran.
        XCTAssertEqual(host.provisions, 0)
        XCTAssertEqual(host.sessions, 0)
    }

    /// A retry after an interruption runs a new chain.
    func testARetryAfterAnInterruptionRunsANewChain() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host
        let started = expectation(description: "the session started")
        host.onSessionStarted = { started.fulfill() }

        lifecycle.start()
        await fulfillment(of: [started], timeout: 5)
        await lifecycle.interrupt()

        let retried = expectation(description: "the session started again")
        host.onSessionStarted = { retried.fulfill() }
        lifecycle.retry()
        await fulfillment(of: [retried], timeout: 5)

        XCTAssertEqual(host.sessions, 2)
        XCTAssertEqual(lifecycle.state, .running)

        await lifecycle.interrupt()
        XCTAssertEqual(host.teardowns, 2)
    }

    /// A retry before anything ran is refused.
    func testARetryBeforeAnythingRanIsRefused() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host

        lifecycle.retry()
        try await Task.sleep(nanoseconds: 100_000_000)

        XCTAssertEqual(host.prepares, 0)
        XCTAssertEqual(lifecycle.state, .idle)
    }

    /// An interruption of a machine that never ran changes nothing.
    func testAnInterruptionOfAnIdleMachineChangesNothing() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host

        let interrupted = await lifecycle.interrupt()

        XCTAssertFalse(interrupted, "a machine that never ran was interrupted")
        XCTAssertEqual(lifecycle.state, .idle)
        XCTAssertEqual(host.teardowns, 0)
        XCTAssertEqual(host.persistedTrails, 0)
    }

    /// An interruption after a failure keeps the failure.
    func testAnInterruptionAfterAFailureKeepsTheFailure() async throws {
        let host = FakeHost()
        host.fails = true
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host

        lifecycle.start()
        try await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(lifecycle.state, .failed)

        let interrupted = await lifecycle.interrupt()

        XCTAssertFalse(interrupted, "a failed machine was interrupted")
        XCTAssertEqual(lifecycle.state, .failed)
        // The failure released the session once, and the interruption did
        // not release it a second time.
        XCTAssertEqual(host.teardowns, 1)
        XCTAssertEqual(host.persistedTrails, 0)
    }

    /// A callback of a chain that was stopped does not release the session
    /// of the chain that runs now.
    ///
    /// The callback of the guest that was stopped can arrive after the retry
    /// began. It must not tear down the new session.
    func testAStaleCallbackDoesNotEndTheNewSession() async throws {
        let host = FakeHost()
        let lifecycle = GuestHostLifecycle()
        lifecycle.delegate = host
        let started = expectation(description: "the session started")
        host.onSessionStarted = { started.fulfill() }

        lifecycle.start()
        await fulfillment(of: [started], timeout: 5)
        await lifecycle.interrupt()

        let retried = expectation(description: "the session started again")
        host.onSessionStarted = { retried.fulfill() }
        lifecycle.retry()
        await fulfillment(of: [retried], timeout: 5)

        // The callback of the chain that was stopped arrives late.
        let ended = await lifecycle.endSession(generation: lifecycle.generation - 1)

        XCTAssertFalse(ended, "a stale callback ended the new session")
        XCTAssertEqual(lifecycle.state, .running)
        XCTAssertEqual(host.teardowns, 1)
    }

    /// The reset control belongs to the launch.
    ///
    /// The first chain of a launch uses it, and a chain that starts again -
    /// a retry after an interruption - keeps the state of the run that was
    /// stopped.
    func testTheResetControlIsUsedOncePerLaunch() {
        var control = FixtureResetControl()

        XCTAssertTrue(control.consume(true), "the reset of the launch was not used")
        XCTAssertFalse(control.consume(true), "the reset was used a second time")
        XCTAssertFalse(control.consume(false), "a launch without the reset asked for one")
    }
}
