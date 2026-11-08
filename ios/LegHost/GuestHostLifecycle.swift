import Foundation

/// What the lifecycle needs from the host.
///
/// The lifecycle drives the host through these calls. The host answers them
/// in the order of the chain of one session, and a test answers them with
/// operations it controls, so the transitions can be exercised without a
/// guest.
@MainActor
protocol GuestHostLifecycleDelegate: AnyObject {
    /// Reads the configuration of the run and checks the device.
    func prepareHost() async throws
    /// Provisions the guest root.
    func provisionGuest() async throws
    /// Boots the guest runtime.
    func bootGuest() async throws
    /// Starts the guest session.
    func startGuestSession() async throws
    /// Ends the session and releases the runtime.
    ///
    /// The teardown throws when the session could not be released: a guest
    /// that did not stop would run next to the session that comes next.
    func teardownGuest() async throws
    /// Writes the trail of the session out.
    func persistGuestTrail() async
}

/// The lifecycle of one guest session.
///
/// The lifecycle is a state machine with one gate: the chain of operations
/// that prepares, provisions, boots and starts the session runs to its end
/// or is cancelled, and no other chain starts while it runs. The
/// interruption releases the session exactly once, so the runtime and the
/// callbacks of a stopped session are never released twice.
@MainActor
final class GuestHostLifecycle {
    /// The state of the gate.
    enum State: Equatable {
        /// No chain ran yet.
        case idle
        /// A chain of operations runs.
        case active
        /// The session of the chain is up.
        case running
        /// The session ended.
        case stopped
        /// The app went to the background while the chain or the session ran.
        case interrupted
        /// The chain failed.
        case failed
    }

    /// The state of the gate.
    private(set) var state: State = .idle

    /// The host the lifecycle drives.
    weak var delegate: GuestHostLifecycleDelegate?

    /// The chain that runs, if one runs.
    private var chain: Task<Void, Never>?

    /// Whether the teardown of the current chain already ran.
    private var didTearDown = false

    /// Whether the session of a chain could not be released.
    ///
    /// A session that could not be released keeps its guest alive, so no
    /// other session starts next to it: the retry is refused then.
    private var releaseFailed = false

    /// Whether a retry waits for the active failing chain to finish unwinding.
    private var retryRequested = false

    /// The number of the chain that owns the callbacks of the runtime.
    ///
    /// The number tells a callback of the runtime which chain it belongs
    /// to: a callback of a chain that was stopped belongs to a session that
    /// is gone, and is ignored.
    private(set) var generation = 0

    /// Whether a chain runs right now.
    var isActive: Bool { chain != nil }

    /// Starts a chain of operations.
    ///
    /// The chain runs to its end. A chain that already runs is not started
    /// again, and a machine that ran a chain before is not started again
    /// either: a retry is the way back from a failure, a stop or an
    /// interruption.
    func start() {
        guard chain == nil, state == .idle else { return }
        begin()
    }

    /// Starts a chain again after a failure, a stop or an interruption.
    ///
    /// A retry from any other state is refused: a chain that runs is never
    /// replaced, and a machine that never ran is started with `start`. If a
    /// retry is requested while a failing chain is still unwinding, it starts
    /// after that chain has released its resources.
    ///
    /// A session that could not be released is not retried either: its guest
    /// is still alive, and another session must not run next to it.
    func retry() {
        guard !releaseFailed else { return }
        switch state {
        case .failed, .stopped, .interrupted:
            guard chain == nil else {
                retryRequested = true
                return
            }
            begin()
        case .active:
            retryRequested = true
        case .idle, .running:
            return
        }
    }

    /// Interrupts the session: the app went to the background.
    ///
    /// The chain is cancelled, the trail of the stopped turn is written out
    /// and the session is released, all before this returns. The session is
    /// released exactly once, so the runtime and the callbacks of the
    /// stopped session are never released twice.
    ///
    /// The result says whether a session was interrupted. A machine that
    /// never ran, one that failed and one that stopped already are left
    /// alone: their state is what the user has to see.
    ///
    /// A session that could not be released is not interrupted either: the
    /// guest of the session is still alive, and the host shows the failure
    /// of the release instead.
    @discardableResult
    func interrupt() async -> Bool {
        // Only a chain that runs or a session that is up can be
        // interrupted: the scene of the app is in the background before it
        // is active the first time, and a session that already stopped is
        // released once and for all.
        guard state == .active || state == .running else { return false }
        let running = chain
        running?.cancel()
        if let running {
            await running.value
        }
        // The callbacks of the stopped chain belong to a session that is
        // gone: the generation moves on, so a callback that arrives late is
        // ignored.
        generation += 1
        await delegate?.persistGuestTrail()
        let released = await tearDown()
        switch state {
        case .active, .running:
            state = released ? .interrupted : .failed
        default:
            break
        }
        return released
    }

    /// The session ended: the guest program is gone.
    ///
    /// The generation says which chain the callback belongs to. A callback of
    /// a chain that was stopped arrives late, after a retry began, and must
    /// not release the session of the chain that runs now.
    ///
    /// The result says whether the session was ended. A callback of an older
    /// chain ends nothing.
    @discardableResult
    func endSession(generation: Int) async -> Bool {
        guard generation == self.generation else { return false }
        let released = await tearDown()
        if state == .running {
            state = released ? .stopped : .failed
        }
        return true
    }

    /// Releases the session of the current chain, once.
    ///
    /// The result says whether the session was released. A session that
    /// could not be released is not released at all: the guest of the
    /// session is still alive, and another session must not run next to it.
    private func tearDown() async -> Bool {
        guard !didTearDown else { return !releaseFailed }
        didTearDown = true
        do {
            try await delegate?.teardownGuest()
            return true
        } catch {
            releaseFailed = true
            return false
        }
    }

    /// Starts a chain.
    private func begin() {
        generation += 1
        didTearDown = false
        state = .active
        chain = Task { [weak self] in
            guard let self else { return }
            await self.execute()
        }
    }

    /// Runs the chain of operations.
    private func execute() async {
        do {
            try await delegate?.prepareHost()
            try Task.checkCancellation()
            try await delegate?.provisionGuest()
            try Task.checkCancellation()
            try await delegate?.bootGuest()
            try Task.checkCancellation()
            try await delegate?.startGuestSession()
            try Task.checkCancellation()
            state = .running
        } catch is CancellationError {
            // The chain was interrupted. The release of the session belongs
            // to the interruption, which writes the trail out before the
            // runtime is released.
            state = .interrupted
        } catch {
            state = .failed
            _ = await tearDown()
        }
        chain = nil
        guard retryRequested, state == .failed, !releaseFailed else {
            retryRequested = false
            return
        }
        retryRequested = false
        begin()
    }
}
