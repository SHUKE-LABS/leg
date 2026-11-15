import Foundation
import Darwin

/// The host-side endpoint of the fixed Unix transport used by `leg-bridge`.
///
/// Darwin has no Linux-style abstract Unix socket namespace, so the listener
/// uses a private app-temporary socket file. The path never enters the guest
/// or the request; the iSH shim maps the one reserved guest name to this
/// per-process endpoint. One connection carries one bounded JSON envelope.
final class ShortcutsBridgeServer {
    static let maximumFrameBytes = 64 * 1024
    static let maximumWaitMilliseconds = 30_000

    private let queue = DispatchQueue(label: "com.shukelabs.leg.shortcuts-bridge",
                                       qos: .userInitiated)
    private let clientQueue = DispatchQueue(label: "com.shukelabs.leg.shortcuts-bridge.clients",
                                             qos: .userInitiated)
    private let stateLock = NSLock()
    private let handler: (ShortcutBridgeRequest) async -> ShortcutBridgeReply
    private let socketPath: String
    private var listener: Int32 = -1
    private var running = false

    init(socketName: String = "leg-host-bridge-\(ProcessInfo.processInfo.processIdentifier)",
         handler: @escaping (ShortcutBridgeRequest) async -> ShortcutBridgeReply) {
        self.socketPath = FileManager.default.temporaryDirectory
            .appendingPathComponent(socketName).path
        self.handler = handler
    }

    /// The private path passed to the iSH socket shim. It is not exposed in
    /// the guest environment or accepted from a bridge request.
    var hostSocketName: String { socketPath }

    func start() throws {
        stateLock.lock()
        let alreadyRunning = listener >= 0
        stateLock.unlock()
        guard !alreadyRunning else { return }
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw ShortcutsBridgeServerError.socket(errno) }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(socketPath.utf8)
        let pathCapacity = MemoryLayout.size(ofValue: address.sun_path)
        guard bytes.count + 1 <= pathCapacity else {
            Darwin.close(fd)
            throw ShortcutsBridgeServerError.nameTooLong
        }
        withUnsafeMutableBytes(of: &address.sun_path) { storage in
            for (index, byte) in bytes.enumerated() {
                storage[index] = byte
            }
            storage[bytes.count] = 0
        }
        let pathOffset = MemoryLayout<sockaddr_un>.offset(of: \.sun_path) ?? 0
        let length = socklen_t(pathOffset + bytes.count + 1)
        #if canImport(Darwin)
        withUnsafeMutableBytes(of: &address) { storage in
            storage[0] = UInt8(length)
        }
        #endif
        removeSocketFile()
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, length)
            }
        }
        guard result == 0 else {
            let error = errno
            Darwin.close(fd)
            removeSocketFile()
            throw ShortcutsBridgeServerError.bind(error)
        }
        guard Darwin.listen(fd, 4) == 0 else {
            let error = errno
            Darwin.close(fd)
            removeSocketFile()
            throw ShortcutsBridgeServerError.listen(error)
        }
        stateLock.lock()
        listener = fd
        running = true
        stateLock.unlock()
        queue.async { [weak self] in self?.acceptLoop() }
    }

    func stop() {
        stateLock.lock()
        running = false
        let fd = listener
        listener = -1
        stateLock.unlock()
        if fd >= 0 { Darwin.close(fd) }
        removeSocketFile()
    }

    private func acceptLoop() {
        while true {
            stateLock.lock()
            let shouldContinue = running
            let fd = listener
            stateLock.unlock()
            guard shouldContinue, fd >= 0 else { return }
            let connection = Darwin.accept(fd, nil, nil)
            if connection < 0 {
                stateLock.lock()
                let stillRunning = running
                stateLock.unlock()
                if !stillRunning { return }
                if errno == EINTR { continue }
                continue
            }
            self.clientQueue.async { [weak self] in self?.serve(connection) }
        }
    }

    private func serve(_ connection: Int32) {
        defer { Darwin.close(connection) }
        disableSigpipe(on: connection)
        let deadline = DispatchTime.now().uptimeNanoseconds
            + UInt64(Self.maximumWaitMilliseconds) * 1_000_000
        guard let frame = readFrame(connection, deadline: deadline),
              let request = try? JSONDecoder().decode(ShortcutBridgeRequest.self, from: frame)
        else { return }

        let semaphore = DispatchSemaphore(value: 0)
        let response = ResponseBox()
        let task = Task { [handler] in
            let reply = await handler(request)
            response.set(reply.data)
            semaphore.signal()
        }
        while true {
            let now = DispatchTime.now().uptimeNanoseconds
            if now >= deadline {
                task.cancel()
                return
            }
            let remaining = max(1, min(50, Int((deadline - now) / 1_000_000)))
            if semaphore.wait(timeout: .now() + .milliseconds(remaining)) == .success {
                break
            }
            var poller = pollfd(fd: connection,
                                events: Int16(POLLIN | POLLHUP | POLLERR),
                                revents: 0)
            let result = Darwin.poll(&poller, 1, 0)
            if result > 0 && (poller.revents & Int16(POLLHUP | POLLERR | POLLNVAL)) != 0 {
                task.cancel()
                return
            }
            // A disconnected guest is noticed above and cancels the native
            // invocation before it can continue unattended.
        }
        guard let responseData = response.get() else { return }
        _ = sendFrame(responseData, on: connection, deadline: deadline)
    }

    private func readFrame(_ fd: Int32, deadline: UInt64) -> Data? {
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while data.count <= Self.maximumFrameBytes {
            let now = DispatchTime.now().uptimeNanoseconds
            guard now < deadline else { return nil }
            let remaining = min(Self.maximumWaitMilliseconds,
                                Int((deadline - now) / 1_000_000))
            var poller = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            let result = Darwin.poll(&poller, 1, Int32(max(1, remaining)))
            guard result > 0 else { return nil }
            let count = buffer.withUnsafeMutableBytes { storage -> Int in
                guard let base = storage.baseAddress else { return -1 }
                return Darwin.recv(fd, base, storage.count, 0)
            }
            guard count > 0 else { return nil }
            data.append(buffer, count: count)
            if let newline = data.firstIndex(of: 0x0A) {
                let frame = data[..<newline]
                guard frame.count <= Self.maximumFrameBytes else { return nil }
                return Data(frame)
            }
        }
        return nil
    }

    @discardableResult
    private func sendFrame(_ data: Data, on fd: Int32, deadline: UInt64) -> Bool {
        guard data.count <= Self.maximumFrameBytes else { return false }
        var frame = data
        frame.append(0x0A)
        var sentAll = true
        frame.withUnsafeBytes { bytes in
            guard let base = bytes.baseAddress else {
                sentAll = false
                return
            }
            var sent = 0
            while sent < frame.count {
                let now = DispatchTime.now().uptimeNanoseconds
                guard now < deadline else {
                    sentAll = false
                    return
                }
                let remaining = min(Self.maximumWaitMilliseconds,
                                    Int((deadline - now) / 1_000_000))
                var poller = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
                guard Darwin.poll(&poller, 1, Int32(max(1, remaining))) > 0 else {
                    sentAll = false
                    return
                }
                let count = Darwin.send(fd, base.advanced(by: sent), frame.count - sent, 0)
                guard count > 0 else {
                    sentAll = false
                    return
                }
                sent += count
            }
        }
        return sentAll
    }

    private func removeSocketFile() {
        socketPath.withCString { _ = Darwin.unlink($0) }
    }

    private func disableSigpipe(on fd: Int32) {
        var enabled: Int32 = 1
        _ = withUnsafePointer(to: &enabled) { pointer in
            Darwin.setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, pointer,
                              socklen_t(MemoryLayout<Int32>.size))
        }
    }

    /// Synchronizes the one response value shared by the async handler and
    /// the transport queue. The semaphore orders the wait, but the lock also
    /// keeps Swift's concurrency checker from treating the capture as an
    /// unsynchronized mutable local.
    private final class ResponseBox: @unchecked Sendable {
        private let lock = NSLock()
        private var data: Data?

        func set(_ data: Data?) {
            lock.lock()
            self.data = data
            lock.unlock()
        }

        func get() -> Data? {
            lock.lock()
            defer { lock.unlock() }
            return data
        }
    }
}
enum ShortcutsBridgeServerError: LocalizedError, Equatable {
    case socket(Int32)
    case bind(Int32)
    case listen(Int32)
    case nameTooLong

    var errorDescription: String? {
        switch self {
        case .socket(let error): return "The Shortcut bridge socket could not be created (\(error))."
        case .bind(let error): return "The Shortcut bridge socket could not be bound (\(error))."
        case .listen(let error): return "The Shortcut bridge socket could not listen (\(error))."
        case .nameTooLong: return "The Shortcut bridge socket name is too long."
        }
    }
}
