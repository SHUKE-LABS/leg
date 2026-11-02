import Foundation
import WebKit

/// The phases the guest host goes through.
enum GuestHostPhase: Equatable {
    case preparing
    case provisioning
    case booting
    case running
    case exited(Int)
    case failed(String)
    /// The app went to the background while the session ran, so the session
    /// was stopped at the boundary of the host.
    case interrupted
}

/// The size the guest terminal has, in character cells.
struct GuestTerminalSize: Equatable {
    /// The smallest terminal the guest program works in.
    static let minimumColumns = 80
    static let minimumRows = 24

    var columns = 0
    var rows = 0

    var isEmpty: Bool { columns <= 0 || rows <= 0 }
}

/// The keys a terminal program reads, but the software keyboard cannot send.
///
/// The host offers them in a row above the terminal, so the guest program
/// can be driven without a hardware keyboard.
enum GuestTerminalKey {
    static let escape = "\u{1b}"
    static let tab = "\t"
    static let up = "\u{1b}[A"
    static let down = "\u{1b}[B"
    static let right = "\u{1b}[C"
    static let left = "\u{1b}[D"
    /// The interrupt.
    static let interrupt = "\u{3}"
    /// The send. The guest TUI submits the composer with it.
    static let submit = "\u{13}"
}

/// The terminal the guest output is written to.
@MainActor
protocol GuestTerminalSink: AnyObject {
    func write(_ text: String)
    func reportSize()
}

/// A failure the host can show to the user.
struct GuestHostError: LocalizedError {
    let message: String

    var errorDescription: String? { message }
}

/// The launch controls of one run of the host.
///
/// The provider, the fixture and the fault controls are DEBUG-only: they
/// configure one run in memory and never write to the Keychain, to the user
/// defaults or to the file system.
struct HostLaunchOptions {
    /// The fixture scenario, when the run is a fixture run.
    var fixtureScenario: LoopbackFixture.Scenario?
    /// The provider the run is configured for.
    var provider: HostProvider?
    /// Whether the run has no provider configuration at all.
    var hasNoProvider = false
    /// The guest program the session runs.
    var program = HostLaunchOptions.defaultProgram
    /// Whether the run reports the runtime files as missing.
    var runtimeFilesMissing = false
    /// Whether the run reports the runtime files as corrupt.
    var runtimeFilesCorrupt = false
    /// Whether the run reports an architecture the guest cannot run on.
    var unsupportedArchitecture = false
    /// Whether the run reports the container as too small for the import.
    var lowStorage = false
    /// Whether the run resets the state of the fixture.
    var fixtureReset = false

    /// The guest program the host runs.
    static let defaultProgram = "/usr/local/bin/leg-tui"
    /// The guest program that is not there, for the runtime failure run.
    static let missingProgram = "/usr/local/bin/leg-missing-fixture-program"
    /// The provider program of the pinned bundle.
    static let legProgram = "/usr/local/bin/leg"
    /// The supervisor program of the pinned bundle.
    static let supervisorProgram = "/usr/local/bin/leg-ui-supervisor"

    /// The options of the current run.
    static func current(arguments: [String] = ProcessInfo.processInfo.arguments) -> HostLaunchOptions {
        var options = HostLaunchOptions()
        var index = 0
        while index < arguments.count {
            let argument = arguments[index]
            var consumed = 1
            switch argument {
            case "-leghost-program":
                if index + 1 < arguments.count {
                    options.program = arguments[index + 1]
                    consumed = 2
                }
            #if DEBUG
            case "-leghost-no-provider":
                options.hasNoProvider = true
            case "-leghost-provider":
                if index + 1 < arguments.count {
                    options.provider = HostProvider(rawValue: arguments[index + 1])
                    consumed = 2
                }
            case "-leghost-fixture":
                if index + 1 < arguments.count {
                    options.fixtureScenario = LoopbackFixture.Scenario(rawValue: arguments[index + 1])
                    consumed = 2
                }
            case "-leghost-runtime-failure":
                options.program = HostLaunchOptions.missingProgram
            case "-leghost-runtime-missing":
                options.runtimeFilesMissing = true
            case "-leghost-runtime-corrupt":
                options.runtimeFilesCorrupt = true
            case "-leghost-unsupported-architecture":
                options.unsupportedArchitecture = true
            case "-leghost-low-storage":
                options.lowStorage = true
            case "-leghost-fixture-reset":
                options.fixtureReset = true
            #endif
            default:
                break
            }
            index += consumed
        }
        return options
    }
}

/// What the host did in one run.
///
/// The state holds counts and the fixture validation result. It carries no
/// request data and no credential, so it can be shown on the screen and read
/// by the tests.
struct HostState: Equatable {
    /// The number of times the guest root was provisioned.
    var provisions = 0
    /// The number of times the guest was booted.
    var boots = 0
    /// The number of times a guest session was started.
    var sessions = 0
    /// The number of times the loopback fixture was started.
    var listeners = 0
    /// The number of requests the durable ledger of the fixture holds.
    var requests = 0
    /// The HTTP statuses the fixture answered with, in order.
    var statuses: [Int] = []
    /// The fixture validation result.
    var validation = "none"
    /// The number of records the exchange trail of the guest holds.
    var trail = 0

    /// The state as the tests read it.
    var summary: String {
        let statuses = self.statuses.isEmpty
            ? ""
            : self.statuses.map(String.init).joined(separator: ",")
        return "provision=\(provisions),boot=\(boots),session=\(sessions),requests=\(requests),"
            + "statuses=\(statuses),validation=\(validation),listener=\(listeners),trail=\(trail)"
    }
}

/// Keeps C strings alive for as long as the C structures that point at them.
private final class CStringStorage {
    private var buffers: [UnsafeMutablePointer<CChar>] = []

    func cString(_ text: String) -> UnsafePointer<CChar> {
        let bytes = Array(text.utf8CString)
        let buffer = UnsafeMutablePointer<CChar>.allocate(capacity: bytes.count)
        buffer.update(from: bytes, count: bytes.count)
        buffers.append(buffer)
        return UnsafePointer(buffer)
    }

    /// A copy of the bytes of `text` with a NUL at the end. The text may hold
    /// NUL characters itself, which is how the argument vector and the
    /// environment are passed to the kernel.
    func cBuffer(_ text: String) -> UnsafePointer<CChar> {
        var bytes = Array(text.utf8).map { CChar(bitPattern: $0) }
        bytes.append(0)
        let buffer = UnsafeMutablePointer<CChar>.allocate(capacity: bytes.count)
        buffer.update(from: bytes, count: bytes.count)
        buffers.append(buffer)
        return UnsafePointer(buffer)
    }

    deinit {
        for buffer in buffers {
            buffer.deallocate()
        }
    }
}

/// The pinned inputs, as they are written down in the manifest.
struct HostPins {
    struct Input {
        let name: String
        let url: String
        let filename: String
        let sha256: String
    }

    struct Compatibility {
        let target: String
        let architecture: String
        let os: String
        let libc: String
        let alpineRelease: String
        let dockerPlatform: String
    }

    let inputs: [Input]
    let compatibility: Compatibility

    /// Reads the pin manifest that ships in the app bundle.
    static func load() throws -> HostPins {
        guard let url = Bundle.main.url(forResource: "PinnedInputs", withExtension: "plist"),
              let data = try? Data(contentsOf: url),
              let plist = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any],
              let rawInputs = plist["inputs"] as? [[String: Any]],
              let rawCompatibility = plist["compatibility"] as? [String: Any]
        else {
            throw GuestHostError(message: "The pinned input manifest is missing from the app bundle.")
        }

        let inputs = try rawInputs.map { entry -> Input in
            guard let name = entry["name"] as? String,
                  let url = entry["url"] as? String,
                  let filename = entry["filename"] as? String,
                  let sha256 = entry["sha256"] as? String
            else {
                throw GuestHostError(message: "The pinned input manifest has an incomplete entry.")
            }
            return Input(name: name, url: url, filename: filename, sha256: sha256)
        }
        guard inputs.count == Int(GUEST_INPUT_COUNT) else {
            throw GuestHostError(message: "The pinned input manifest does not list both guest inputs.")
        }

        func value(_ key: String) throws -> String {
            guard let text = rawCompatibility[key] as? String else {
                throw GuestHostError(message: "The pinned input manifest has no \(key).")
            }
            return text
        }

        let compatibility = Compatibility(
            target: try value("target"),
            architecture: try value("architecture"),
            os: try value("os"),
            libc: try value("libc"),
            alpineRelease: try value("alpine_release"),
            dockerPlatform: try value("docker_platform"))
        return HostPins(inputs: inputs, compatibility: compatibility)
    }

    /// The file of a pinned input in the app bundle.
    static func embeddedInputURL(named filename: String) throws -> URL {
        guard let url = Bundle.main.url(forResource: filename, withExtension: nil,
                                        subdirectory: "PinnedInputs")
                ?? Bundle.main.url(forResource: filename, withExtension: nil) else {
            throw GuestHostError(message: "The pinned input \(filename) is missing from the app bundle.")
        }
        return url
    }

    /// Runs `body` with the pins in the shape the provisioner expects.
    func withCPins<R>(_ body: (UnsafePointer<guest_pins>) -> R) -> R {
        let storage = CStringStorage()
        var cInputs = inputs.map { input in
            guest_pinned_input(name: storage.cString(input.name),
                               url: storage.cString(input.url),
                               filename: storage.cString(input.filename),
                               sha256: storage.cString(input.sha256))
        }
        var cCompatibility = guest_compatibility(
            target: storage.cString(compatibility.target),
            architecture: storage.cString(compatibility.architecture),
            os: storage.cString(compatibility.os),
            libc: storage.cString(compatibility.libc),
            alpine_release: storage.cString(compatibility.alpineRelease),
            docker_platform: storage.cString(compatibility.dockerPlatform))
        var cPins = guest_pins(inputs: nil, compatibility: nil)
        return withExtendedLifetime(storage) {
            cInputs.withUnsafeMutableBufferPointer { inputBuffer in
                withUnsafePointer(to: &cCompatibility) { compatibilityPointer in
                    cPins.inputs = UnsafePointer(inputBuffer.baseAddress!)
                    cPins.compatibility = compatibilityPointer
                    return withUnsafePointer(to: &cPins) { body($0) }
                }
            }
        }
    }
}

/// Turns the bytes the guest wrote into the string the terminal expects.
///
/// hterm decodes the string it receives as UTF-8, so every byte has to reach
/// the terminal as one character with the same value. The bytes are therefore
/// kept as they are and not decoded here.
enum GuestOutputText {
    static func latin1(_ bytes: [UInt8]) -> String {
        String(bytes: bytes, encoding: .isoLatin1) ?? ""
    }
}

/// The flag that tells the output thread of a session to leave its loop.
///
/// The flag is a box of its own, so the thread of an earlier session cannot
/// see the flag of a later one and keeps on reading the ring buffer.
private final class GuestOutputFlag {
    private let lock = NSLock()
    private var running = true

    var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return running
    }

    func stop() {
        lock.lock()
        running = false
        lock.unlock()
    }
}

/// The guest host: it provisions the pinned root, boots the vendored iSH
/// runtime and keeps the terminal in sync with the guest.
@MainActor
final class GuestHostController: NSObject, ObservableObject, WKScriptMessageHandler,
                                 GuestHostLifecycleDelegate {
    @Published private(set) var phase: GuestHostPhase = .preparing
    @Published private(set) var detail: String = ""
    @Published private(set) var reusedVerifiedRoot = false
    /// The size the guest terminal was told about, in character cells.
    @Published private(set) var terminalSize = GuestTerminalSize()
    /// What the host did in this run.
    @Published private(set) var state = HostState()

    /// The stable diagnostic the host shows when the guest runtime cannot run
    /// the guest program.
    nonisolated static let runtimeErrorDiagnostic = "IOS_GUEST_RUNTIME_ERROR"
    /// The stable diagnostic the host shows when the system interrupted the
    /// session.
    nonisolated static let interruptionDiagnostic = "IOS_SESSION_INTERRUPTED"

    /// The machine that runs the chain of operations of one session.
    private let lifecycle = GuestHostLifecycle()

    private weak var sink: GuestTerminalSink?
    private let outputSignal = DispatchSemaphore(value: 0)
    private var outputThread: Thread?
    /// The flag of the thread that drains the guest output.
    private var outputFlag: GuestOutputFlag?
    private var terminalReady = false
    /// The output the guest produced before the terminal was ready. It is
    /// written to the terminal as soon as it loads, so the first screen of
    /// the guest program is not lost.
    private var pendingOutput = ""

    /// The launch controls of this run.
    private var options = HostLaunchOptions.current()

    /// Whether the run is a fixture run.
    ///
    /// A fixture run offers the send control on the screen: the software
    /// keyboard cannot send the key the guest TUI submits the composer with.
    var isFixtureRun: Bool { options.fixtureScenario != nil }
    /// The provider configuration this run uses.
    private var configuration: ProviderConfiguration?
    /// The pins of the guest inputs this run uses.
    private var pins: HostPins?
    /// The root the guest runs in.
    private var activeRoot = ""
    /// The loopback fixture, in a fixture run.
    private var fixture: LoopbackFixture?
    /// The durable ledger of the requests the fixture accepted.
    private var ledger: FixtureRequestLedger?
    /// The file the raw terminal output is written to, in a fixture run.
    private var transcriptHandle: FileHandle?
    /// The watcher of the guest's exchange trail, in a fixture run.
    private var trailWatcher: DispatchSourceTimer?

    /// The environment every guest session gets.
    private static let baseEnvironment = [
        "TERM=xterm-256color",
        "HOME=/root",
        "PATH=/usr/local/bin:/usr/bin:/bin",
        "SHELL=/bin/sh",
    ]

    /// The values a fixture run pins, so the turn is deterministic.
    ///
    /// The zero retry budget makes the number of provider requests the same
    /// on every run, and the short timeouts keep a stuck turn from outliving
    /// the test.
    private static let fixtureEnvironment = [
        "LEG_MAX_TOKENS=256",
        "LEG_MAX_TOOL_ROUNDS=1",
        "LEG_MAX_RETRIES=0",
        "LEG_RETRY_BASE_DELAY_MS=0",
        "LEG_TIMEOUT_SECS=5",
        "LEG_STREAM_IDLE_TIMEOUT_SECS=5",
        "LEG_BASH_TIMEOUT_SECS=5",
        "LEG_EVENT_LOG=/tmp/leg-ios-fixture/exchange.jsonl",
        "NO_COLOR=1",
    ]

    // MARK: - Lifecycle

    /// Provisions the guest and starts the session.
    func start() {
        lifecycle.delegate = self
        lifecycle.start()
    }

    /// Starts the session again after a failure, a stop or an interruption.
    ///
    /// The turn that was running is not sent again: the guest program is
    /// started from the state it kept, and the user sends the turn again.
    func retry() {
        lifecycle.delegate = self
        lifecycle.retry()
    }

    /// The app went to the background.
    ///
    /// iOS does not run the guest in the background, so the session stops at
    /// a boundary the host controls: the chain is cancelled, the trail of the
    /// stopped turn is written out, and the runtime and the callbacks of the
    /// session are released exactly once.
    func handleBackground() {
        lifecycle.delegate = self
        Task { [weak self] in
            guard let self else { return }
            await self.lifecycle.interrupt()
            self.publishInterruption()
        }
    }

    /// The app came back to the foreground.
    ///
    /// The session stays where the interruption left it. The host shows the
    /// interrupted state and the trail of the stopped turn, and the user
    /// starts a new session with the retry control. The prompt of the turn
    /// that was running is not sent again.
    func handleForeground() {
        refreshFixtureState()
    }

    /// Shows the interruption of the session.
    private func publishInterruption() {
        phase = .interrupted
        detail = "\(Self.interruptionDiagnostic): the session was stopped when the app went to "
            + "the background. iOS does not run the guest in the background, so the turn that was "
            + "running was not finished and its prompt is not sent again. The exchange trail of "
            + "the stopped turn was kept with \(state.trail) records. Start the host again to "
            + "reopen the verified root."
    }

    // MARK: - The chain of the lifecycle

    /// Reads the configuration of the run and checks the device.
    ///
    /// The provider configuration is read before the host touches the guest
    /// root: a missing or an unsupported configuration has to be reported
    /// before anything is provisioned. The preflight runs next and reports
    /// the state of the device the same way.
    func prepareHost() async throws {
        phase = .preparing
        options = HostLaunchOptions.current()
        do {
            detail = "Reading the provider configuration"
            let configuration = try ProviderConfigurationStore.load(
                providerOverride: options.provider,
                hasNoProvider: options.hasNoProvider)
            if options.fixtureScenario == nil && configuration.baseURL.isEmpty {
                throw ProviderConfigurationError.missing
            }
            self.configuration = configuration

            detail = "Checking the guest runtime files"
            let pins: HostPins
            do {
                pins = try HostPins.load()
            } catch {
                // The manifest of the pins is one of the runtime files: a
                // manifest that cannot be read means the runtime files are
                // not there.
                throw HostPreflightDiagnostic.runtimeFilesMissing
            }
            try HostPreflight.check(HostPreflightProviders.live(options: options, pins: pins))
            self.pins = pins
            openLedger()
        } catch {
            publishFailure(error)
            throw error
        }
    }

    /// Provisions the guest root.
    func provisionGuest() async throws {
        do {
            try Self.removeProvisionedRootIfRequested()
            phase = .provisioning
            detail = "Verifying the pinned guest inputs"
            guard let pins else {
                throw HostPreflightDiagnostic.runtimeFilesMissing
            }
            let provisioned = try await provisionRoot(pins)
            activeRoot = provisioned.activeRoot
            state.provisions += 1
            reusedVerifiedRoot = provisioned.reused
            detail = provisioned.reused ? "Reusing the verified guest root" : "Imported the guest root"
        } catch {
            publishFailure(error)
            throw error
        }
    }

    /// Boots the guest runtime and starts the fixture.
    func bootGuest() async throws {
        do {
            phase = .booting
            detail = "Booting the guest"
            if options.fixtureScenario != nil {
                // The guest appends its exchange trail to a file, so the
                // directory of the trail has to be there before the session
                // starts.
                try prepareFixtureState(root: activeRoot)
                watchExchangeTrail(root: activeRoot)
            }
            try await bootRuntime(root: activeRoot)
            state.boots += 1
            watchGuestExit()

            // The fixture is the provider the guest talks to. It starts after
            // the configuration is accepted and before the session, so the
            // guest reaches it from its first request.
            if let scenario = options.fixtureScenario, let configuration {
                try startFixture(scenario: scenario, configuration: configuration)
            }
        } catch {
            publishFailure(error)
            throw error
        }
    }

    /// Starts the guest session.
    func startGuestSession() async throws {
        do {
            try await startSession()
            state.sessions += 1
            startOutputPump()
            phase = .running
            detail = reusedVerifiedRoot ? "Guest running (reused root)" : "Guest running"
        } catch {
            publishFailure(error)
            throw error
        }
    }

    /// Ends the session and releases the runtime.
    ///
    /// The teardown runs exactly once per chain of the lifecycle: the
    /// callbacks of the runtime are detached, the fixture and the watcher of
    /// the trail are stopped, and the runtime releases the terminal of the
    /// guest.
    func teardownGuest() async {
        guest_runtime_set_output_notify(nil, nil)
        guest_runtime_set_exit_notify(nil, nil)
        outputFlag?.stop()
        outputFlag = nil
        outputThread = nil
        fixture?.stop()
        fixture = nil
        trailWatcher?.cancel()
        trailWatcher = nil
        guest_runtime_shutdown()
    }

    /// Writes the trail of the session out.
    ///
    /// The trail of the guest lives in the guest root. It is copied into the
    /// test artifacts of the app container, so the trail of a turn that was
    /// interrupted is kept outside the guest root as well.
    func persistGuestTrail() async {
        guard let trail = exchangeTrailURL(), let data = try? Data(contentsOf: trail) else { return }
        state.trail = data.split(separator: 0x0A).count
        guard let destination = try? Self.persistedTrailURL() else { return }
        try? data.write(to: destination, options: .atomic)
    }

    /// Shows a failure of the run.
    private func publishFailure(_ error: Error) {
        let message = error.localizedDescription
        // The diagnostic of the guest runtime belongs in the terminal as
        // well: the guest program never ran, so the terminal is the only
        // place where the run can show what went wrong. The transcript of
        // a fixture run holds it as well.
        if message.contains(Self.runtimeErrorDiagnostic) {
            writeToTerminal("\r\n\(message)\r\n")
        }
        phase = .failed(message)
        detail = message
    }

    // MARK: - Provisioning

    private struct Provisioned {
        let reused: Bool
        let activeRoot: String
    }

    private func provisionRoot(_ pins: HostPins) async throws -> Provisioned {
        let bundle = try HostPins.embeddedInputURL(named: pins.inputs[GUEST_INPUT_LEG_BUNDLE].filename)
        let alpine = try HostPins.embeddedInputURL(named: pins.inputs[GUEST_INPUT_ALPINE_ROOT].filename)
        let roots = try Self.rootsDirectory()

        return try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                var result = guest_provision_result()
                var message = [CChar](repeating: 0, count: 1024)
                let status = pins.withCPins { pinsPointer in
                    bundle.path.withCString { bundlePointer in
                        alpine.path.withCString { alpinePointer in
                            roots.path.withCString { rootsPointer in
                                guest_provision(pinsPointer, bundlePointer, alpinePointer, rootsPointer,
                                                nil, &result, &message, message.count)
                            }
                        }
                    }
                }
                guard status == Int32(GUEST_PROVISION_OK.rawValue) else {
                    let reason = String(cString: message)
                    continuation.resume(throwing: GuestHostError(
                        message: reason.isEmpty ? String(cString: guest_provision_status_string(status)) : reason))
                    return
                }
                let activeRoot = withUnsafeBytes(of: result.active_root) { raw -> String in
                    String(cString: raw.bindMemory(to: CChar.self).baseAddress!)
                }
                continuation.resume(returning: Provisioned(reused: result.reused, activeRoot: activeRoot))
            }
        }
    }

    private static func rootsDirectory() throws -> URL {
        let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                 appropriateFor: nil, create: true)
        let roots = support.appendingPathComponent("LegHost/roots", isDirectory: true)
        try FileManager.default.createDirectory(at: roots, withIntermediateDirectories: true)
        return roots
    }

    /// The directory the artifacts of a run are written to.
    static func testArtifactsDirectory() throws -> URL {
        let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                 appropriateFor: nil, create: true)
        let artifacts = support.appendingPathComponent("LegHost/test-artifacts", isDirectory: true)
        try FileManager.default.createDirectory(at: artifacts, withIntermediateDirectories: true)
        return artifacts
    }

    /// The file the trail of the session is written to.
    static func persistedTrailURL() throws -> URL {
        try testArtifactsDirectory().appendingPathComponent("exchange-trail.jsonl")
    }

    /// The file the durable ledger of the requests is written to.
    static func requestLedgerURL() throws -> URL {
        try testArtifactsDirectory().appendingPathComponent("request-ledger.jsonl")
    }

    /// Removes the provisioned root when the app was started with
    /// `-leghost-reset`. The simulator tests use it to start from a known
    /// state, and it is the way to import the guest root again without
    /// removing the app.
    private static func removeProvisionedRootIfRequested() throws {
        guard ProcessInfo.processInfo.arguments.contains("-leghost-reset") else { return }
        let roots = try rootsDirectory()
        if FileManager.default.fileExists(atPath: roots.path) {
            try FileManager.default.removeItem(at: roots)
        }
    }

    /// Opens the durable ledger of the requests the fixture accepts.
    ///
    /// The ledger is cleared on the reset launch only. An ordinary relaunch
    /// counts on top of the requests of the run before it, so a run that
    /// starts again cannot hide a duplicate behind a fresh counter.
    private func openLedger() {
        guard options.fixtureScenario != nil else { return }
        guard let url = try? Self.requestLedgerURL() else { return }
        let ledger = FixtureRequestLedger(url: url)
        if options.fixtureReset {
            try? ledger.clear()
        }
        self.ledger = ledger
    }

    // MARK: - Runtime

    /// The state a fixture run starts from.
    ///
    /// The state the guest program keeps between runs is the catalog of the
    /// guest program, so it asks for a workspace again, and the exchange
    /// trail of the last run. Both are removed on the reset launch only: an
    /// ordinary relaunch keeps the workspace and the trail of the run before
    /// it, so a session can be picked up where it stopped.
    private func prepareFixtureState(root: String) throws {
        let data = URL(fileURLWithPath: root).appendingPathComponent("data")
        let trailDirectory = data
            .appendingPathComponent("tmp")
            .appendingPathComponent("leg-ios-fixture")
        let trail = trailDirectory.appendingPathComponent("exchange.jsonl")
        if options.fixtureReset {
            let guestState = data
                .appendingPathComponent("root")
                .appendingPathComponent(".local")
                .appendingPathComponent("state")
                .appendingPathComponent("leg-ui")
            if FileManager.default.fileExists(atPath: guestState.path) {
                try FileManager.default.removeItem(at: guestState)
            }
            if FileManager.default.fileExists(atPath: trail.path) {
                try FileManager.default.removeItem(at: trail)
            }
        }
        // The guest appends to the trail file and does not make its
        // directory, so the directory is there before the session starts.
        try FileManager.default.createDirectory(at: trailDirectory, withIntermediateDirectories: true)
    }

    /// The trail of the exchange of the guest.
    private func exchangeTrailURL() -> URL? {
        guard !activeRoot.isEmpty else { return nil }
        return URL(fileURLWithPath: activeRoot)
            .appendingPathComponent("data")
            .appendingPathComponent("tmp")
            .appendingPathComponent("leg-ios-fixture")
            .appendingPathComponent("exchange.jsonl")
    }

    /// Watches the exchange trail of the guest for the failure of the turn.
    ///
    /// The guest program writes the trail as it goes, so the host sees the
    /// failure of a turn while the run is still going, and the number of
    /// records the trail holds. The watcher stops when it finds the failure,
    /// or when the run ends.
    private func watchExchangeTrail(root: String) {
        let trail = URL(fileURLWithPath: root)
            .appendingPathComponent("data")
            .appendingPathComponent("tmp")
            .appendingPathComponent("leg-ios-fixture")
            .appendingPathComponent("exchange.jsonl")
        let seen = GuestTrailCounter()
        let timer = DispatchSource.makeTimerSource(queue: DispatchQueue.global(qos: .utility))
        timer.schedule(deadline: .now() + .milliseconds(200), repeating: .milliseconds(200))
        timer.setEventHandler { [weak self, weak timer] in
            let snapshot = GuestHostController.trailSnapshot(in: trail)
            if let failure = snapshot.failure {
                timer?.cancel()
                DispatchQueue.main.async { self?.reportTurnFailure(failure) }
                return
            }
            guard seen.update(snapshot.records) else { return }
            DispatchQueue.main.async { self?.publishTrailRecords(snapshot.records) }
        }
        trailWatcher = timer
        timer.resume()
    }

    /// Publishes the number of records the trail of the guest holds.
    private func publishTrailRecords(_ records: Int) {
        // A trail that was written out at the boundary of the session stays
        // as it was: a watcher that is still running must not change it.
        guard phase != .interrupted else { return }
        state.trail = records
    }

    /// Writes the failure the guest reported for the turn into the terminal.
    ///
    /// The guest program of the pinned bundle does not render the failure of
    /// a turn in the iOS host: the turn never ends there. The guest does
    /// report the failure, in its exchange trail, and the host writes that
    /// report into the terminal, where the guest speaks. The transcript of
    /// the run holds it as well, because the transcript records what the
    /// terminal showed.
    private func reportTurnFailure(_ failure: String) {
        writeToTerminal("\r\n\(failure)\r\n")
    }

    /// What the host reads out of the exchange trail of the guest.
    nonisolated private struct TrailSnapshot {
        /// The number of records the trail holds.
        var records: Int
        /// The failure the guest reported, when it reported one.
        var failure: String?
    }

    /// Reads the trail of the guest.
    ///
    /// The trail holds one JSON object per line. A failed turn records a
    /// `response_error` line, and that line holds the message the guest
    /// reported.
    nonisolated private static func trailSnapshot(in trail: URL) -> TrailSnapshot {
        guard let data = try? Data(contentsOf: trail),
              let text = String(data: data, encoding: .utf8) else {
            return TrailSnapshot(records: 0, failure: nil)
        }
        let lines = text.split(separator: "\n")
        var failure: String?
        for line in lines {
            guard let record = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                  record["event"] as? String == "response_error",
                  let message = record["message"] as? String, !message.isEmpty else { continue }
            failure = message
        }
        return TrailSnapshot(records: lines.count, failure: failure)
    }

    private func bootRuntime(root: String) async throws {
        let dataDirectory = root + "/data"
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            DispatchQueue.global(qos: .userInitiated).async {
                let status = dataDirectory.withCString { guest_runtime_boot($0) }
                if status < 0 {
                    continuation.resume(throwing: GuestHostError(
                        message: "The guest root could not be mounted (error \(status))."))
                } else {
                    continuation.resume()
                }
            }
        }
    }

    /// Reports the end of the guest program to the user.
    private func watchGuestExit() {
        let generation = lifecycle.generation
        guest_runtime_set_exit_notify({ status, context in
            guard let context = context else { return }
            let controller = Unmanaged<GuestHostController>.fromOpaque(context).takeUnretainedValue()
            DispatchQueue.main.async { controller.guestDidExit(Int(status), generation: generation) }
        }, Unmanaged.passUnretained(self).toOpaque())
    }

    /// The guest program ended. `status` is the wait status of the process: an
    /// exit code in the high byte, or the signal that stopped it.
    ///
    /// A callback of a session that was stopped belongs to a session that is
    /// gone, so it is ignored.
    private func guestDidExit(_ status: Int, generation: Int) {
        guard generation == lifecycle.generation else { return }
        let exitCode = status >> 8
        let signal = status & 0x7f
        Task { [weak self] in
            guard let self else { return }
            await self.lifecycle.endSession()
            if exitCode != 0 {
                self.phase = .exited(status)
                self.detail = "The guest program exited with code \(exitCode)."
            } else if signal != 0 {
                self.phase = .exited(status)
                self.detail = "The guest program was stopped by signal \(signal)."
            } else {
                self.phase = .exited(status)
                self.detail = "The guest program exited."
            }
        }
    }

    private func startSession() async throws {
        let program = options.program
        let guestArguments = self.guestArguments(for: program)
        let guestEnvironment = self.guestEnvironment()
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            DispatchQueue.global(qos: .userInitiated).async {
                // The kernel takes the argument vector and the environment as
                // one buffer of NUL separated strings each. The buffers have
                // to stay alive until the kernel has copied them into the new
                // process, so they are built inside the closure that runs the
                // session.
                let storage = CStringStorage()
                let arguments = storage.cBuffer(guestArguments.joined(separator: "\0") + "\0")
                let environment = storage.cBuffer(guestEnvironment.joined(separator: "\0") + "\0")

                let status = withExtendedLifetime(storage) {
                    program.withCString { programPointer in
                        guest_runtime_start_session(programPointer, arguments,
                                                   guestArguments.count, environment)
                    }
                }
                if status < 0 {
                    continuation.resume(throwing: GuestHostError(
                        message: "\(Self.runtimeErrorDiagnostic): the guest program could not be "
                            + "started (error \(status))."))
                } else {
                    continuation.resume()
                }
            }
        }
    }

    // MARK: - Environment

    /// The arguments the guest program runs with.
    ///
    /// The guest program finds its companion programs next to its own
    /// executable, and it looks for its own executable through
    /// `/proc/self/exe`. The emulator cannot answer that read for the process
    /// it runs, and the answer takes the whole host down, so the host names the
    /// two programs of the pinned bundle instead. A guest program that is
    /// not the guest TUI takes no arguments.
    private func guestArguments(for program: String) -> [String] {
        guard program == HostLaunchOptions.defaultProgram else { return [program] }
        return [program,
                "--leg-bin", HostLaunchOptions.legProgram,
                "--supervisor-bin", HostLaunchOptions.supervisorProgram]
    }

    /// The environment the guest session runs with.
    ///
    /// The list is an allowlist. The guest gets the terminal settings, the
    /// provider configuration and, in a fixture run, the values that pin the
    /// turn down. Nothing else of the host environment reaches the guest, and
    /// the credential is never written down.
    private func guestEnvironment() -> [String] {
        guard let configuration else { return Self.baseEnvironment }
        let isFixture = options.fixtureScenario != nil
        let baseURL = fixture?.baseURL ?? configuration.baseURL
        let model = isFixture ? ProviderConfigurationStore.fixtureModel : configuration.model
        let window = isFixture
            ? ProviderConfigurationStore.fixtureContextWindowTokens
            : configuration.contextWindowTokens
        var environment = Self.baseEnvironment
        environment.append("LEG_PROVIDER=\(configuration.provider.rawValue)")
        environment.append("ANTHROPIC_API_KEY=\(configuration.apiKey)")
        environment.append("ANTHROPIC_BASE_URL=\(baseURL)")
        environment.append("LEG_MODEL=\(model)")
        environment.append("LEG_CONTEXT_WINDOW_TOKENS=\(window)")
        if isFixture {
            environment.append(contentsOf: Self.fixtureEnvironment(for: options.fixtureScenario))
        }
        return environment
    }

    /// The values that pin a fixture turn down.
    ///
    /// The delayed scenario holds its answer back, so the turn has to outlast
    /// the timeout of the guest as well.
    private static func fixtureEnvironment(for scenario: LoopbackFixture.Scenario?) -> [String] {
        var environment = fixtureEnvironment
        guard scenario == .delayed else { return environment }
        environment.removeAll { $0.hasPrefix("LEG_TIMEOUT_SECS=")
            || $0.hasPrefix("LEG_STREAM_IDLE_TIMEOUT_SECS=") }
        environment.append("LEG_TIMEOUT_SECS=120")
        environment.append("LEG_STREAM_IDLE_TIMEOUT_SECS=120")
        return environment
    }

    // MARK: - Fixture

    /// Starts the loopback fixture and the terminal transcript.
    private func startFixture(scenario: LoopbackFixture.Scenario,
                               configuration: ProviderConfiguration) throws {
        let fixture = LoopbackFixture(scenario: scenario,
                                      expectedAPIKey: configuration.apiKey,
                                      ledger: ledger)
        try fixture.start()
        fixture.onObservationChanged = { [weak self] in
            DispatchQueue.main.async { self?.refreshFixtureState() }
        }
        self.fixture = fixture
        state.listeners += 1
        try openTranscript()
        refreshFixtureState()
    }

    /// Reads the fixture observation into the published state.
    private func refreshFixtureState() {
        guard let fixture else { return }
        let observation = fixture.currentObservation
        // The number of requests is the number the durable ledger holds: a
        // run that starts again counts the requests of the run before it.
        state.requests = ledger?.count ?? observation.acceptedRequests
        state.statuses = observation.statuses
        state.validation = observation.validation
    }

    /// Opens the file the raw terminal output is written to.
    ///
    /// The transcript is written in a fixture run only. It holds the bytes the
    /// guest produced, so it is written the way the guest produced them.
    private func openTranscript() throws {
        let transcript = try Self.testArtifactsDirectory()
            .appendingPathComponent("terminal-transcript.log")
        FileManager.default.createFile(atPath: transcript.path, contents: nil)
        transcriptHandle = try FileHandle(forWritingTo: transcript)
    }

    /// Writes the raw guest output to the transcript.
    private func writeToTranscript(_ text: String) {
        guard let transcriptHandle, let data = text.data(using: .isoLatin1) else { return }
        transcriptHandle.seekToEndOfFile()
        try? transcriptHandle.write(contentsOf: data)
    }

    // MARK: - Output

    private func startOutputPump() {
        let flag = GuestOutputFlag()
        outputFlag = flag
        guest_runtime_set_output_notify({ context in
            guard let context else { return }
            let semaphore = Unmanaged<DispatchSemaphore>.fromOpaque(context).takeUnretainedValue()
            semaphore.signal()
        }, Unmanaged.passUnretained(outputSignal).toOpaque())

        let signal = outputSignal
        let thread = Thread { [weak self] in
            var buffer = [UInt8](repeating: 0, count: 16 * 1024)
            while flag.isRunning {
                let length = guest_runtime_read_output(&buffer, buffer.count)
                if length == 0 {
                    _ = signal.wait(timeout: .now() + .milliseconds(100))
                    continue
                }
                let text = GuestOutputText.latin1(Array(buffer[0..<length]))
                guard !text.isEmpty else { continue }
                DispatchQueue.main.async { [weak self] in
                    self?.writeToTerminal(text)
                }
            }
        }
        thread.stackSize = 512 * 1024
        outputThread = thread
        thread.start()
    }

    private func writeToTerminal(_ text: String) {
        writeToTranscript(text)
        guard terminalReady else {
            // The terminal is not ready yet: hold the output back, so the
            // first screen of the guest program is not lost.
            pendingOutput += text
            return
        }
        sink?.write(text)
    }

    // MARK: - Input

    /// Sends text to the guest, the way the software keyboard does.
    func sendInput(_ text: String) {
        var normalized = text.replacingOccurrences(of: "\n", with: "\r")
        if normalized.isEmpty { return }
        let bytes = Array(normalized.utf8)
        let written = normalized.withCString { pointer in
            guest_runtime_write_input(pointer, strlen(pointer))
        }
        NSLog("[LegHost] forwarded %d of %d bytes to the guest: %@",
              written, bytes.count,
              bytes.prefix(16).map { String(format: "%02x", $0) }.joined(separator: " "))
    }

    /// Reports the terminal size to the guest.
    ///
    /// The software keyboard covers half of the screen, so the terminal can be
    /// smaller than the guest program can work in. The guest keeps the
    /// smallest terminal it can use, so a program that needs room does not
    /// stop working while the keyboard is up.
    func resize(columns: Int, rows: Int) {
        guard columns > 0, rows > 0 else { return }
        let guestColumns = max(columns, GuestTerminalSize.minimumColumns)
        let guestRows = max(rows, GuestTerminalSize.minimumRows)
        terminalSize = GuestTerminalSize(columns: guestColumns, rows: guestRows)
        guest_runtime_resize(Int32(guestColumns), Int32(guestRows))
    }

    // MARK: - Terminal

    func attach(sink: GuestTerminalSink) {
        self.sink = sink
        if terminalReady {
            sink.reportSize()
        }
    }

    func userContentController(_ userContentController: WKUserContentController,
                               didReceive message: WKScriptMessage) {
        switch message.name {
        case "load":
            terminalReady = true
            if !pendingOutput.isEmpty {
                sink?.write(pendingOutput)
                pendingOutput = ""
            }
            sink?.reportSize()
        case "log":
            if let text = message.body as? String {
                NSLog("[hterm] %@", text)
            }

        case "sendInput":
            if let text = message.body as? String {
                sendInput(text)
            }
        case "resize":
            sink?.reportSize()
        case "focus":
            NotificationCenter.default.post(name: .guestTerminalNeedsFocus, object: nil)
        case "propUpdate":
            break
        case "syncFocus", "newScrollHeight", "newScrollTop":
            break
        case "openLink":
            break
        default:
            break
        }
    }
}

/// The number of records the watcher of the trail saw last.
///
/// The watcher reads the trail on its own queue, so the number it compares
/// against is kept in a box of its own.
private final class GuestTrailCounter {
    private var records = -1

    /// Remembers the number of records and reports whether it changed.
    func update(_ records: Int) -> Bool {
        guard records != self.records else { return false }
        self.records = records
        return true
    }
}

extension Notification.Name {
    /// Posted when the terminal wants the keyboard, for example after a tap.
    static let guestTerminalNeedsFocus = Notification.Name("guestTerminalNeedsFocus")
}
