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

/// The guest host: it provisions the pinned root, boots the vendored iSH
/// runtime and keeps the terminal in sync with the guest.
@MainActor
final class GuestHostController: NSObject, ObservableObject, WKScriptMessageHandler {
    @Published private(set) var phase: GuestHostPhase = .preparing
    @Published private(set) var detail: String = ""
    @Published private(set) var reusedVerifiedRoot = false
    /// The size the guest terminal was told about, in character cells.
    @Published private(set) var terminalSize = GuestTerminalSize()

    private weak var sink: GuestTerminalSink?
    private let outputSignal = DispatchSemaphore(value: 0)
    private var outputThread: Thread?
    private var terminalReady = false
    /// The output the guest produced before the terminal was ready. It is
    /// written to the terminal as soon as it loads, so the first screen of
    /// the guest program is not lost.
    private var pendingOutput = ""

    /// The guest program the host runs.
    private static let guestProgram = "/usr/local/bin/leg-tui"
    private static let guestEnvironment = [
        "TERM=xterm-256color",
        "HOME=/root",
        "PATH=/usr/local/bin:/usr/bin:/bin",
        "SHELL=/bin/sh",
    ]

    // MARK: - Lifecycle

    /// Provisions the guest and starts the session.
    func start() {
        Task { await prepare() }
    }

    /// Removes the provisioned root and provisions it again.
    func reset() {
        Task {
            do {
                let roots = try Self.rootsDirectory()
                try FileManager.default.removeItem(at: roots)
                phase = .preparing
                detail = "The guest root was removed."
                await prepare()
            } catch {
                phase = .failed("The guest root could not be removed: \(error.localizedDescription)")
            }
        }
    }

    private func prepare() async {
        do {
            phase = .preparing
            try Self.removeProvisionedRootIfRequested()
            phase = .provisioning
            detail = "Verifying the pinned guest inputs"
            let pins = try HostPins.load()
            let provisioned = try await provision(pins)
            reusedVerifiedRoot = provisioned.reused
            detail = provisioned.reused ? "Reusing the verified guest root" : "Imported the guest root"

            phase = .booting
            detail = "Booting the guest"
            try await boot(root: provisioned.activeRoot)
            watchGuestExit()
            try await startSession()
            startOutputPump()
            phase = .running
            detail = provisioned.reused ? "Guest running (reused root)" : "Guest running"
        } catch {
            phase = .failed(error.localizedDescription)
            detail = error.localizedDescription
        }
    }

    // MARK: - Provisioning

    private struct Provisioned {
        let reused: Bool
        let activeRoot: String
    }

    private func provision(_ pins: HostPins) async throws -> Provisioned {
        let bundle = try Self.embeddedInputURL(named: pins.inputs[GUEST_INPUT_LEG_BUNDLE].filename)
        let alpine = try Self.embeddedInputURL(named: pins.inputs[GUEST_INPUT_ALPINE_ROOT].filename)
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

    private static func embeddedInputURL(named filename: String) throws -> URL {
        guard let url = Bundle.main.url(forResource: filename, withExtension: nil,
                                        subdirectory: "PinnedInputs")
                ?? Bundle.main.url(forResource: filename, withExtension: nil) else {
            throw GuestHostError(message: "The pinned input \(filename) is missing from the app bundle.")
        }
        return url
    }

    private static func rootsDirectory() throws -> URL {
        let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                 appropriateFor: nil, create: true)
        let roots = support.appendingPathComponent("LegHost/roots", isDirectory: true)
        try FileManager.default.createDirectory(at: roots, withIntermediateDirectories: true)
        return roots
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

    // MARK: - Runtime

    private func boot(root: String) async throws {
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
        guest_runtime_set_exit_notify({ status, context in
            guard let context = context else { return }
            let controller = Unmanaged<GuestHostController>.fromOpaque(context).takeUnretainedValue()
            DispatchQueue.main.async { controller.guestDidExit(Int(status)) }
        }, Unmanaged.passUnretained(self).toOpaque())
    }

    /// The guest program ended. `status` is the wait status of the process: an
    /// exit code in the high byte, or the signal that stopped it.
    private func guestDidExit(_ status: Int) {
        let exitCode = status >> 8
        let signal = status & 0x7f
        if exitCode != 0 {
            phase = .exited(status)
            detail = "The guest program exited with code \(exitCode)."
        } else if signal != 0 {
            phase = .exited(status)
            detail = "The guest program was stopped by signal \(signal)."
        } else {
            phase = .exited(status)
            detail = "The guest program exited."
        }
    }

    private func startSession() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            DispatchQueue.global(qos: .userInitiated).async {
                // The kernel takes the argument vector and the environment as
                // one buffer of NUL separated strings each. The buffers have
                // to stay alive until the kernel has copied them into the new
                // process, so they are built inside the closure that runs the
                // session.
                let storage = CStringStorage()
                let arguments = storage.cBuffer(Self.guestProgram + "\0")
                let environment = storage.cBuffer(Self.guestEnvironment.joined(separator: "\0") + "\0")

                let status = withExtendedLifetime(storage) {
                    Self.guestProgram.withCString { program in
                        guest_runtime_start_session(program, arguments, 1, environment)
                    }
                }
                if status < 0 {
                    continuation.resume(throwing: GuestHostError(
                        message: "The guest program could not be started (error \(status))."))
                } else {
                    continuation.resume()
                }
            }
        }
    }

    // MARK: - Output

    private func startOutputPump() {
        guest_runtime_set_output_notify({ context in
            guard let context = context else { return }
            let semaphore = Unmanaged<DispatchSemaphore>.fromOpaque(context).takeUnretainedValue()
            semaphore.signal()
        }, Unmanaged.passUnretained(outputSignal).toOpaque())

        let signal = outputSignal
        let thread = Thread { [weak self] in
            var buffer = [UInt8](repeating: 0, count: 16 * 1024)
            while true {
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
        NSLog("[LegHost] forwarding %d bytes to the guest", normalized.utf8.count)
        normalized.withCString { pointer in
            guest_runtime_write_input(pointer, strlen(pointer))
        }
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

extension Notification.Name {
    /// Posted when the terminal wants the keyboard, for example after a tap.
    static let guestTerminalNeedsFocus = Notification.Name("guestTerminalNeedsFocus")
}
