import Foundation
import Network

/// The deterministic Anthropic Messages fixture the host serves on loopback.
///
/// The fixture lets one guest turn run without a provider: it serves
/// `POST /v1/messages` on the loopback interface and answers with a fixed
/// stream. It records counts, statuses and the validation result only. A
/// request header, a request body and the credential are read in memory to
/// validate a request and are never written down or logged.
final class LoopbackFixture {
    /// The scenarios the fixture can play.
    enum Scenario: String {
        /// One successful turn: a text block, one bash tool call and the
        /// final text.
        case success
        /// The provider answers the first request with a server error.
        case providerError = "provider-error"
        /// The listener is closed before the turn, so the guest cannot reach
        /// the fixture at all.
        case unreachable
        /// The fixture holds the answer back, so a turn stays running long
        /// enough for the host to be interrupted in the middle of it.
        case delayed
    }

    /// The text the fixture streams in the first answer.
    static let streamText = "IOS_FIXTURE_STREAM_TEXT"
    /// The command the fixture asks the guest to run.
    static let toolCommand = "printf IOS_FIXTURE_TOOL_RESULT"
    /// The text the fixture expects in the tool result of the second request.
    static let toolResult = "IOS_FIXTURE_TOOL_RESULT"
    /// The text the fixture streams in the final answer.
    static let finalResponse = "IOS_FIXTURE_FINAL_RESPONSE"
    /// The error type the provider error carries.
    static let providerErrorType = "api_error"
    /// The message the provider error carries.
    static let providerErrorMessage = "IOS_FIXTURE_PROVIDER_ERROR"

    /// What the fixture observed.
    ///
    /// The observation holds counts, statuses and the validation result. It
    /// carries no request data and no credential.
    struct Observation: Equatable {
        /// The number of accepted requests.
        var acceptedRequests = 0
        /// The HTTP statuses of the accepted requests, in order.
        var statuses: [Int] = []
        /// The validation result: `ok` or a short reason.
        var validation = "none"
    }

    /// The scenario the fixture plays.
    let scenario: Scenario
    /// The credential the guest has to send.
    private let expectedAPIKey: String
    /// The durable record of the requests the fixture accepted.
    ///
    /// The ledger is optional: a run without one keeps its counts in
    /// memory only.
    private let ledger: FixtureRequestLedger?
    /// The port the fixture listens on.
    private(set) var port: UInt16 = 0
    /// Called after the fixture accepted or rejected a request.
    var onObservationChanged: (() -> Void)?

    private let queue = DispatchQueue(label: "com.shukelabs.leg.ioshost.fixture")
    private let lock = NSLock()
    private var listener: NWListener?
    private var connections: [NWConnection] = []
    private var received = Data()
    private var observation = Observation()
    private var rejection: String?
    private var requestNumber = 0
    /// The turn the requests belong to.
    private var turnID = UUID().uuidString
    /// The number of requests the current turn holds.
    private var turnRequestCount = 0

    init(scenario: Scenario, expectedAPIKey: String, ledger: FixtureRequestLedger? = nil) {
        self.scenario = scenario
        self.expectedAPIKey = expectedAPIKey
        self.ledger = ledger
    }

    /// The base URL the guest is pointed at.
    var baseURL: String {
        "http://127.0.0.1:\(port)"
    }

    /// What the fixture observed so far.
    var currentObservation: Observation {
        lock.lock()
        defer { lock.unlock() }
        var result = observation
        if let rejection {
            result.validation = rejection
        } else if let expected = expectedRequestCount, result.acceptedRequests != expected {
            result.validation = "expected \(expected) requests, saw \(result.acceptedRequests)"
        } else {
            result.validation = "ok"
        }
        return result
    }

    /// The number of requests the scenario answers.
    private var expectedRequestCount: Int? {
        switch scenario {
        case .success: return 2
        case .providerError: return 1
        case .unreachable: return 0
        case .delayed: return 2
        }
    }

    /// The time the delayed scenario holds its answer back.
    ///
    /// The delay is longer than the timeout of the guest, so a turn of the
    /// delayed scenario is still running when the host is interrupted.
    static let delayedAnswerDelay: TimeInterval = 120

    // MARK: - Listener

    /// Starts the listener and waits for the port.
    func start() throws {
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = NWEndpoint.hostPort(host: "127.0.0.1", port: .any)
        let listener = try NWListener(using: parameters)
        let ready = DispatchSemaphore(value: 0)
        let outcome = OutcomeBox()
        listener.stateUpdateHandler = { state in
            switch state {
            case .ready:
                ready.signal()
            case .failed(let error):
                outcome.error = error
                ready.signal()
            default:
                break
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            self?.accept(connection)
        }
        self.listener = listener
        listener.start(queue: queue)

        if ready.wait(timeout: .now() + 10) == .timedOut {
            listener.cancel()
            throw GuestHostError(message: "The loopback fixture did not start.")
        }
        if let error = outcome.error {
            listener.cancel()
            throw GuestHostError(message: "The loopback fixture failed: \(error.localizedDescription)")
        }
        port = listener.port?.rawValue ?? 0
        guard port != 0 else {
            listener.cancel()
            throw GuestHostError(message: "The loopback fixture did not get a port.")
        }
        if scenario == .unreachable {
            // The scenario is a closed port: the guest connects to an address
            // nothing listens on.
            listener.cancel()
            self.listener = nil
        }
    }

    /// Stops the listener and the connections it holds.
    func stop() {
        lock.lock()
        let open = connections
        connections = []
        lock.unlock()
        for connection in open {
            connection.cancel()
        }
        listener?.cancel()
        listener = nil
    }

    // MARK: - Requests

    private func accept(_ connection: NWConnection) {
        lock.lock()
        connections.append(connection)
        lock.unlock()
        connection.stateUpdateHandler = { [weak connection] state in
            if case .failed = state {
                connection?.cancel()
            }
        }
        connection.start(queue: queue)
        receive(on: connection)
    }

    private func receive(on connection: NWConnection) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data, !data.isEmpty {
                self.received.append(data)
            }
            if error != nil {
                connection.cancel()
                return
            }
            if let request = self.parseRequest() {
                self.handle(request, on: connection)
                return
            }
            if isComplete {
                connection.cancel()
                return
            }
            self.receive(on: connection)
        }
    }

    /// One request, as the fixture reads it.
    private struct FixtureRequest {
        let method: String
        let path: String
        let headers: [String: String]
        let body: Data
    }

    private func parseRequest() -> FixtureRequest? {
        let separator = Data("\r\n\r\n".utf8)
        guard let headerEnd = received.range(of: separator) else { return nil }
        let headerText = String(decoding: received[..<headerEnd.lowerBound], as: UTF8.self)
        var lines = headerText.components(separatedBy: "\r\n")
        guard !lines.isEmpty else { return nil }
        let requestLine = lines.removeFirst().split(separator: " ", maxSplits: 2).map(String.init)
        guard requestLine.count >= 2 else { return nil }

        var headers: [String: String] = [:]
        for line in lines {
            guard let colon = line.firstIndex(of: ":") else { continue }
            let name = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
            let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            headers[name] = value
        }
        let length = Int(headers["content-length"] ?? "0") ?? 0
        let bodyStart = headerEnd.upperBound
        guard received.count >= bodyStart + length else { return nil }
        let body = Data(received[bodyStart..<(bodyStart + length)])
        return FixtureRequest(method: requestLine[0],
                              path: requestLine[1].components(separatedBy: "?").first ?? requestLine[1],
                              headers: headers,
                              body: body)
    }

    private func handle(_ request: FixtureRequest, on connection: NWConnection) {
        received = Data()
        requestNumber += 1
        let number = requestNumber
        let problems = validate(request, number: number)
        guard problems.isEmpty else {
            reject(problems.joined(separator: ","), on: connection)
            return
        }
        // The request is accepted before the answer is on its way: a run
        // that is interrupted while the answer is coming still shows the
        // request in the ledger.
        switch scenario {
        case .providerError:
            accept(status: 500)
            answer(status: 500, on: connection, body: providerError())
        case .unreachable:
            reject("unexpected request \(number)", on: connection)
        case .success:
            accept(status: 200)
            answerTurn(number: number, on: connection)
        case .delayed:
            accept(status: 200)
            // The answer is held back, so the turn is running long enough
            // for the host to be interrupted in the middle of it.
            queue.asyncAfter(deadline: .now() + Self.delayedAnswerDelay) { [weak self] in
                self?.answerTurn(number: number, on: connection)
            }
        }
    }

    /// Answers the request of a turn: the first one asks for the tool call,
    /// the second one carries its result and ends the turn.
    private func answerTurn(number: Int, on connection: NWConnection) {
        if number == 1 {
            answer(status: 200, on: connection, body: firstAnswer())
        } else if number == 2 {
            answer(status: 200, on: connection, body: finalAnswer())
        } else {
            reject("unexpected request \(number)", on: connection)
        }
    }

    /// Records an accepted request.
    ///
    /// The count of the run and the durable ledger grow here, so the two
    /// always hold the same number of requests.
    private func accept(status: Int) {
        lock.lock()
        observation.acceptedRequests += 1
        observation.statuses.append(status)
        let expected = expectedRequestCount ?? Int.max
        if turnRequestCount >= expected {
            // The turn is complete, so the next request starts a new one.
            turnID = UUID().uuidString
            turnRequestCount = 0
        }
        turnRequestCount += 1
        let entry = FixtureRequestLedger.Entry(turn: turnID, request: turnRequestCount, status: status)
        lock.unlock()
        try? ledger?.append(turn: entry.turn, request: entry.request, status: entry.status)
        onObservationChanged?()
    }

    /// The reasons the request is not the one the fixture expects.
    ///
    /// The reasons name the field that did not match, never the value: the
    /// credential and the request body stay out of the observation.
    private func validate(_ request: FixtureRequest, number: Int) -> [String] {
        var problems: [String] = []
        if request.method != "POST" {
            problems.append("method")
        }
        if request.path != "/v1/messages" {
            problems.append("path")
        }
        if request.headers["x-api-key"] != expectedAPIKey {
            problems.append("api-key")
        }
        guard let parsed = try? JSONSerialization.jsonObject(with: request.body),
              let object = parsed as? [String: Any]
        else {
            problems.append("body")
            return problems
        }
        if (object["stream"] as? Bool) != true {
            problems.append("stream")
        }
        if number == 2 && !carriesToolResult(object) {
            problems.append("tool-result")
        }
        return problems
    }

    /// Whether the request carries the tool result of the fixture's call.
    private func carriesToolResult(_ object: [String: Any]) -> Bool {
        guard let messages = object["messages"] as? [[String: Any]] else { return false }
        for message in messages {
            guard let blocks = message["content"] as? [Any] else { continue }
            for case let block as [String: Any] in blocks {
                guard (block["type"] as? String) == "tool_result" else { continue }
                if let content = block["content"] as? String, content.contains(Self.toolResult) {
                    return true
                }
            }
        }
        return false
    }

    // MARK: - Answers

    private func answer(status: Int, on connection: NWConnection, body: String) {
        let contentType = status == 200 ? "text/event-stream; charset=utf-8" : "application/json"
        respond(status: status, contentType: contentType, body: Data(body.utf8), on: connection)
    }

    private func reject(_ reason: String, on connection: NWConnection) {
        lock.lock()
        if rejection == nil {
            rejection = reason
        }
        lock.unlock()
        onObservationChanged?()
        let body = #"{"type":"error","error":{"type":"invalid_request_error","message":"IOS_FIXTURE_REQUEST_REJECTED"}}"#
        respond(status: 400, contentType: "application/json", body: Data(body.utf8), on: connection)
    }

    private func respond(status: Int, contentType: String, body: Data, on connection: NWConnection) {
        var head = "HTTP/1.1 \(status) \(Self.reason(for: status))\r\n"
        head += "Content-Type: \(contentType)\r\n"
        head += "Content-Length: \(body.count)\r\n"
        head += "Connection: close\r\n\r\n"
        var data = Data(head.utf8)
        data.append(body)
        connection.send(content: data, completion: .contentProcessed { _ in
            connection.cancel()
        })
    }

    private static func reason(for status: Int) -> String {
        switch status {
        case 200: return "OK"
        case 400: return "Bad Request"
        case 500: return "Internal Server Error"
        default: return "Status"
        }
    }

    /// The first answer: a text block and one bash tool call.
    private func firstAnswer() -> String {
        var body = sse("message_start", """
        {"type":"message_start","message":{"id":"msg_ios_fixture_1","type":"message",\
        "role":"assistant","content":[],"model":"claude-ios-fixture","stop_reason":null,\
        "usage":{"input_tokens":1,"output_tokens":0}}}
        """)
        body += sse("content_block_start", """
        {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
        """)
        body += sse("content_block_delta", """
        {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"\(Self.streamText)"}}
        """)
        body += sse("content_block_stop", #"{"type":"content_block_stop","index":0}"#)
        body += sse("content_block_start", """
        {"type":"content_block_start","index":1,"content_block":{"type":"tool_use",\
        "id":"toolu_ios_fixture_1_1","name":"bash","input":{}}}
        """)
        body += sse("content_block_delta", """
        {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta",\
        "partial_json":"{\\"command\\":\\"\(Self.toolCommand)\\"}"}}
        """)
        body += sse("content_block_stop", #"{"type":"content_block_stop","index":1}"#)
        body += sse("message_delta", """
        {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},\
        "usage":{"output_tokens":1}}
        """)
        body += sse("message_stop", #"{"type":"message_stop"}"#)
        return body
    }

    /// The final answer: the text that ends the turn.
    private func finalAnswer() -> String {
        var body = sse("message_start", """
        {"type":"message_start","message":{"id":"msg_ios_fixture_2","type":"message",\
        "role":"assistant","content":[],"model":"claude-ios-fixture","stop_reason":null,\
        "usage":{"input_tokens":1,"output_tokens":0}}}
        """)
        body += sse("content_block_start", """
        {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
        """)
        body += sse("content_block_delta", """
        {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"\(Self.finalResponse)"}}
        """)
        body += sse("content_block_stop", #"{"type":"content_block_stop","index":0}"#)
        body += sse("message_delta", """
        {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},\
        "usage":{"output_tokens":1}}
        """)
        body += sse("message_stop", #"{"type":"message_stop"}"#)
        return body
    }

    /// The provider error body.
    private func providerError() -> String {
        #"{"type":"error","error":{"type":"\#(Self.providerErrorType)","message":"\#(Self.providerErrorMessage)"}}"#
    }

    /// One server-sent event.
    private func sse(_ name: String, _ data: String) -> String {
        "event: \(name)\ndata: \(data)\n\n"
    }
}

/// The result the listener hands back to the thread that waits for it.
private final class OutcomeBox {
    var error: Error?
}
