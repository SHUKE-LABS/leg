import XCTest
import Darwin
@testable import LegHost

/// Focused tests for the host-owned Shortcut registration and bridge envelope.
@MainActor
final class ShortcutTests: XCTestCase {
    func testRegistrationsUseExactNamesAndPersistTheirInputKind() throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Morning report", inputKind: .json)
        XCTAssertThrowsError(try store.save(name: "Morning report", inputKind: .text)) { error in
            XCTAssertEqual(error as? ShortcutRegistrationError, .duplicateName)
        }
        try store.save(name: "morning report", inputKind: .text)

        XCTAssertEqual(store.registration(named: "Morning report")?.inputKind, .json)
        XCTAssertEqual(store.registration(named: "morning report")?.inputKind, .text)
        XCTAssertNil(store.registration(named: "Morning Report"))
        XCTAssertEqual(try ShortcutRegistrationStore.load(defaults: defaults).count, 2)
    }

    func testMalformedRegistrationStoreIsReadOnlyAndNotSilentlyReplaced() throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let key = "leg.shortcuts.registrations.v1"
        let malformed = Data("not-json".utf8)
        defaults.set(malformed, forKey: key)

        let store = ShortcutRegistrationStore(defaults: defaults)

        XCTAssertEqual(store.loadError, .invalidStore)
        XCTAssertTrue(store.registrations.isEmpty)
        XCTAssertThrowsError(try store.save(name: "Lost registration", inputKind: .text)) { error in
            XCTAssertEqual(error as? ShortcutRegistrationError, .invalidStore)
        }
        XCTAssertThrowsError(try store.remove(name: "Lost registration")) { error in
            XCTAssertEqual(error as? ShortcutRegistrationError, .invalidStore)
        }
        XCTAssertEqual(defaults.data(forKey: key), malformed)
    }

    func testRegistrationNamesRejectEmptyControlAndOversizedValues() {
        XCTAssertThrowsError(try ShortcutRegistrationStore.validateName("")) { error in
            XCTAssertEqual(error as? ShortcutRegistrationError, .emptyName)
        }
        XCTAssertThrowsError(try ShortcutRegistrationStore.validateName("bad\nname")) { error in
            XCTAssertEqual(error as? ShortcutRegistrationError, .controlCharacter)
        }
        XCTAssertThrowsError(try ShortcutRegistrationStore.validateName(
            String(repeating: "x", count: ShortcutRegistrationStore.maximumNameBytes + 1))) { error in
            XCTAssertEqual(error as? ShortcutRegistrationError, .nameTooLong)
        }
    }

    func testAnUnregisteredNameIsDeniedBeforeConfirmationOrURLDispatch() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        var confirmationCalled = false
        var openerCalled = false
        let adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in
                confirmationCalled = true
                return true
            },
            opener: { _ in
                openerCalled = true
                return true
            })
        let request = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-test-1",
            nonce: "nonce",
            capability: ShortcutsAdapter.capability,
            input: [
                "name": .string("Not registered"),
                "input": .object(["text": .string("hello")]),
            ],
            inputSHA256: String(repeating: "a", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)

        let reply = await adapter.handle(request)
        XCTAssertEqual(reply.status, "denied")
        XCTAssertEqual(reply.error?.reason, "shortcut_not_registered")
        XCTAssertFalse(confirmationCalled)
        XCTAssertFalse(openerCalled)
    }

    func testShortcutURLUsesOnlyThePublicRunEndpointAndCallbacks() throws {
        let url = try XCTUnwrap(ShortcutsAdapter.shortcutURL(
            name: "Morning report",
            input: "{\"format\":\"json\"}",
            callbackState: "callback-state-2"))
        let components = try XCTUnwrap(URLComponents(url: url, resolvingAgainstBaseURL: false))
        let query = Dictionary(uniqueKeysWithValues: (components.queryItems ?? []).map {
            ($0.name, $0.value ?? "")
        })

        XCTAssertEqual(components.scheme, "shortcuts")
        XCTAssertEqual(components.host, "x-callback-url")
        XCTAssertEqual(components.path, "/run-shortcut")
        XCTAssertEqual(query["name"], "Morning report")
        XCTAssertEqual(query["input"], "text")
        XCTAssertEqual(query["text"], "{\"format\":\"json\"}")
        XCTAssertEqual(query["x-success"], "legshortcuts://success?state=callback-state-2")
        XCTAssertEqual(query["x-cancel"], "legshortcuts://cancel?state=callback-state-2")
        XCTAssertEqual(query["x-error"], "legshortcuts://error?state=callback-state-2")
    }

    func testUnknownAndStaleCallbackStatesCannotSettleARequest() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Callback state", inputKind: .text)
        var adapter: ShortcutsAdapter!
        var openedURLs: [URL] = []
        let secondDispatch = expectation(description: "the later request was dispatched")
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                openedURLs.append(url)
                if openedURLs.count == 2 { secondDispatch.fulfill() }
                return true
            })
        func callbackState(in url: URL) -> String? {
            guard let callbackString = URLComponents(url: url,
                                                     resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "x-success" })?.value,
                  let callbackURL = URL(string: callbackString) else { return nil }
            return URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "state" })?.value
        }
        let input: [String: BridgeJSONValue] = [
            "name": .string("Callback state"),
            "input": .object(["text": .string("hello")]),
        ]
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-state-1",
            nonce: "nonce-state-1",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: String(repeating: "5", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)
        let approval = await adapter.handle(first)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)
        adapter.handleCallback(URL(string:
            "legshortcuts://success?state=forged-state&result=forged")!)
        let reply = await adapter.handle(ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-state-2",
            nonce: "nonce-state-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 100))
        XCTAssertEqual(reply.status, "error")
        XCTAssertEqual(reply.error?.reason, "timeout")

        let staleState = try XCTUnwrap(callbackState(in: openedURLs[0]))
        let secondApproval = await adapter.handle(ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-state-3",
            nonce: "nonce-state-3",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: nil,
            timeoutMS: 5_000))
        let secondToken = try XCTUnwrap(
            secondApproval.result?.objectValue?["approval_token"]?.stringValue)
        let secondTask = Task { @MainActor in
            await adapter.handle(ShortcutBridgeRequest(
                version: 1,
                requestID: "shortcut-state-4",
                nonce: "nonce-state-4",
                capability: ShortcutsAdapter.capability,
                input: input,
                inputSHA256: first.inputSHA256,
                approvalToken: secondToken,
                timeoutMS: 5_000))
        }
        await fulfillment(of: [secondDispatch], timeout: 1)
        let currentState = try XCTUnwrap(callbackState(in: openedURLs[1]))

        // The first request's valid state is stale now. It must not settle the
        // later pending request or make its current callback state unusable.
        adapter.handleCallback(URL(string:
            "legshortcuts://success?state=\(staleState)&result=stale")!)
        adapter.handleCallback(URL(string:
            "legshortcuts://success?state=\(currentState)&result=fresh")!)
        let secondReply = await secondTask.value
        XCTAssertEqual(secondReply.status, "ok")
        XCTAssertEqual(secondReply.result?.objectValue?["value"], .string("fresh"))
    }

    func testUserRefusalDoesNotDispatchTheShortcut() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Refused", inputKind: .text)
        var openerCalled = false
        let adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in false },
            opener: { _ in
                openerCalled = true
                return true
            })
        let request = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-refused-1",
            nonce: "nonce-refused-1",
            capability: ShortcutsAdapter.capability,
            input: [
                "name": .string("Refused"),
                "input": .object(["text": .string("hello")]),
            ],
            inputSHA256: String(repeating: "d", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)

        let reply = await adapter.handle(request)
        XCTAssertEqual(reply.status, "denied")
        XCTAssertEqual(reply.error?.reason, "user_refused")
        XCTAssertFalse(openerCalled)
    }

    func testApprovalDigestIsRecomputedBeforeAChangedInputCanRedeemIt() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Digest bound", inputKind: .text)
        var openerCalled = false
        let adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { _ in
                openerCalled = true
                return true
            })
        let approvedInput: [String: BridgeJSONValue] = [
            "name": .string("Digest bound"),
            "input": .object(["text": .string("approved")]),
        ]
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-digest-1",
            nonce: "nonce-digest-1",
            capability: ShortcutsAdapter.capability,
            input: approvedInput,
            inputSHA256: String(repeating: "e", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)

        let approval = await adapter.handle(first)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)
        let changed = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-digest-2",
            nonce: "nonce-digest-2",
            capability: ShortcutsAdapter.capability,
            input: [
                "name": .string("Digest bound"),
                "input": .object(["text": .string("changed")]),
            ],
            // A malicious guest can retain the old caller-supplied digest. The
            // host must use the decoded input instead.
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 5_000)

        let reply = await adapter.handle(changed)
        XCTAssertEqual(reply.status, "denied")
        XCTAssertEqual(reply.error?.reason, "approval_mismatch")
        XCTAssertFalse(openerCalled)
    }

    func testLargeJSONReviewSummaryIsEscapedAndBounded() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Bounded review", inputKind: .json)
        let largeKey = String(repeating: "\"", count: 3_000)
        let adapter = ShortcutsAdapter(registry: store, confirmation: { _, _ in true })
        let request = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-review-1",
            nonce: "nonce-review-1",
            capability: ShortcutsAdapter.capability,
            input: [
                "name": .string("Bounded review"),
                "input": .object([largeKey: .string("value")]),
            ],
            inputSHA256: String(repeating: "f", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)

        let reply = await adapter.handle(request)
        XCTAssertEqual(reply.status, "approval_required")
        let review = try XCTUnwrap(reply.result?.objectValue?["review"])
        XCTAssertLessThanOrEqual(try XCTUnwrap(review.jsonData).count,
                                 ShortcutsAdapter.maximumReviewBytes)
        XCTAssertTrue(review.stringValue == nil)
        XCTAssertTrue(review.objectValue?["input"]?.stringValue?.contains("…") == true)
    }

    func testUnavailableAndCallbackFailureMappingsUseThePublicErrorMessage() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Callback mapping", inputKind: .text)
        var adapter: ShortcutsAdapter!
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                guard let callbackString = URLComponents(url: url,
                                                         resolvingAgainstBaseURL: false)?
                    .queryItems?.first(where: { $0.name == "x-error" })?.value,
                      let callbackURL = URL(string: callbackString),
                      let callbackState = URLComponents(url: callbackURL,
                                                   resolvingAgainstBaseURL: false)?
                        .queryItems?.first(where: { $0.name == "state" })?.value else {
                    return false
                }
                let callback = "legshortcuts://error?state=\(callbackState)&errorMessage=The%20shortcut%20could%20not%20be%20found"
                Task { @MainActor in adapter.handleCallback(URL(string: callback)!) }
                return true
            })
        let input: [String: BridgeJSONValue] = [
            "name": .string("Callback mapping"),
            "input": .object(["text": .string("hello")]),
        ]
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-callback-1",
            nonce: "nonce-callback-1",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: String(repeating: "1", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)
        let approval = await adapter.handle(first)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)
        let followUp = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-callback-2",
            nonce: "nonce-callback-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 5_000)

        let notFound = await adapter.handle(followUp)
        XCTAssertEqual(notFound.status, "unavailable")
        XCTAssertEqual(notFound.error?.reason, "shortcut_not_found")

        let unavailable = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { _ in false })
        let unavailableApproval = await unavailable.handle(first)
        let unavailableToken = try XCTUnwrap(
            unavailableApproval.result?.objectValue?["approval_token"]?.stringValue)
        let unavailableReply = await unavailable.handle(ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-unavailable-2",
            nonce: "nonce-unavailable-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: unavailableToken,
            timeoutMS: 5_000))
        XCTAssertEqual(unavailableReply.status, "unavailable")
        XCTAssertEqual(unavailableReply.error?.reason, "shortcuts_unavailable")
    }

    func testApprovalTokenIsRequiredBeforeDispatchAndReturnsStructuredJSON() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "JSON result", inputKind: .json)
        var adapter: ShortcutsAdapter!
        var openedURL: URL?
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                openedURL = url
                guard let callbackString = URLComponents(url: url,
                                                         resolvingAgainstBaseURL: false)?
                    .queryItems?.first(where: { $0.name == "x-success" })?.value,
                      let callbackURL = URL(string: callbackString),
                    let callbackState = URLComponents(url: callbackURL,
                                                   resolvingAgainstBaseURL: false)?
                        .queryItems?.first(where: { $0.name == "state" })?.value else {
                    return false
                }
                Task { @MainActor in
                    adapter.handleCallback(URL(string:
                        "legshortcuts://success?state=\(callbackState)&result=%7B%22ok%22%3Atrue%7D")!)
                }
                return true
            })
        let input: [String: BridgeJSONValue] = [
            "name": .string("JSON result"),
            "input": .object(["format": .string("json")]),
        ]
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-test-3",
            nonce: "nonce-3",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: String(repeating: "b", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)

        let approval = await adapter.handle(first)
        XCTAssertEqual(approval.status, "approval_required")
        XCTAssertNil(openedURL)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)

        let followUp = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-test-3-follow-up",
            nonce: "nonce-3-follow-up",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 5_000)
        let result = await adapter.handle(followUp)
        XCTAssertEqual(result.status, "ok")
        XCTAssertEqual(result.result?.objectValue?["ok"], .bool(true))
        XCTAssertEqual(URLComponents(url: try XCTUnwrap(openedURL),
                                     resolvingAgainstBaseURL: false)?.host, "x-callback-url")
    }

    func testMissingResultAndCancellationMapToStableStatuses() async throws {
        func callbackStateValue(from url: URL, callbackName: String) -> String? {
            guard let callbackString = URLComponents(url: url,
                                                     resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == callbackName })?.value,
                  let callbackURL = URL(string: callbackString) else { return nil }
            return URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "state" })?.value
        }

        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Outcome mapping", inputKind: .text)
        let input: [String: BridgeJSONValue] = [
            "name": .string("Outcome mapping"),
            "input": .object(["text": .string("hello")]),
        ]

        var missingResultAdapter: ShortcutsAdapter!
        missingResultAdapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                guard let callbackState = callbackStateValue(from: url, callbackName: "x-success") else {
                    return false
                }
                Task { @MainActor in
                    missingResultAdapter.handleCallback(
                        URL(string: "legshortcuts://success?state=\(callbackState)")!)
                }
                return true
            })
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-outcome-1",
            nonce: "nonce-outcome-1",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: String(repeating: "2", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)
        let approval = await missingResultAdapter.handle(first)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)
        let missing = await missingResultAdapter.handle(ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-outcome-2",
            nonce: "nonce-outcome-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 5_000))
        XCTAssertEqual(missing.status, "error")
        XCTAssertEqual(missing.error?.reason, "missing_result")

        var cancelledAdapter: ShortcutsAdapter!
        cancelledAdapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                guard let callbackState = callbackStateValue(from: url, callbackName: "x-cancel") else {
                    return false
                }
                Task { @MainActor in
                    cancelledAdapter.handleCallback(
                        URL(string: "legshortcuts://cancel?state=\(callbackState)")!)
                }
                return true
            })
        let cancelledApproval = await cancelledAdapter.handle(first)
        let cancelledToken = try XCTUnwrap(
            cancelledApproval.result?.objectValue?["approval_token"]?.stringValue)
        let cancelled = await cancelledAdapter.handle(ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-outcome-3",
            nonce: "nonce-outcome-3",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: cancelledToken,
            timeoutMS: 5_000))
        XCTAssertEqual(cancelled.status, "cancelled")
        XCTAssertEqual(cancelled.error?.reason, "cancelled")
    }

    func testCallbackTimeoutUsesTheStableTimeoutReason() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Slow Shortcut", inputKind: .text)
        let adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { _ in true })
        let input: [String: BridgeJSONValue] = [
            "name": .string("Slow Shortcut"),
            "input": .object(["text": .string("hello")]),
        ]
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-timeout-1",
            nonce: "nonce-timeout-1",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: String(repeating: "c", count: 64),
            approvalToken: nil,
            timeoutMS: 100)
        let approval = await adapter.handle(first)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)
        let followUp = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-timeout-2",
            nonce: "nonce-timeout-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 100)

        let reply = await adapter.handle(followUp)
        XCTAssertEqual(reply.status, "error")
        XCTAssertEqual(reply.error?.reason, "timeout")
    }

    func testBridgeServerRoundTripsARealStructuredShortcutResult() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Bridge JSON", inputKind: .json)
        var adapter: ShortcutsAdapter!
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                guard let callbackString = URLComponents(url: url,
                                                         resolvingAgainstBaseURL: false)?
                    .queryItems?.first(where: { $0.name == "x-success" })?.value,
                      let callbackURL = URL(string: callbackString),
                      let callbackState = URLComponents(url: callbackURL,
                                                       resolvingAgainstBaseURL: false)?
                        .queryItems?.first(where: { $0.name == "state" })?.value else {
                    return false
                }
                let callback = "legshortcuts://success?state=\(callbackState)"
                    + "&result=%7B%22ok%22%3Atrue%2C%22count%22%3A2%7D"
                Task { @MainActor in
                    adapter.handleCallback(URL(string: callback)!)
                }
                return true
            })
        let server = ShortcutsBridgeServer(
            socketName: "leg-host-bridge-test-\(UUID().uuidString)") { request in
                await adapter.handle(request)
            }
        try server.start()
        let socketPath = server.hostSocketName
        defer {
            server.stop()
            adapter.cancelAll()
        }

        let input: [String: BridgeJSONValue] = [
            "name": .string("Bridge JSON"),
            "input": .object(["format": .string("json")]),
        ]
        let digest = try XCTUnwrap(ShortcutsAdapter.inputDigest(for: input))
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "bridge-json-1",
            nonce: "bridge-json-nonce-1",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: digest,
            approvalToken: nil,
            timeoutMS: 5_000)
        let firstFrame = try JSONEncoder().encode(first)
        let approvalData = try await Task.detached {
            try Self.sendBridgeFrame(firstFrame, to: socketPath)
        }.value
        let approval = try JSONDecoder().decode(ShortcutBridgeReply.self, from: approvalData)
        XCTAssertEqual(approval.status, "approval_required")
        XCTAssertEqual(approval.requestID, first.requestID)
        let token = try XCTUnwrap(
            approval.result?.objectValue?["approval_token"]?.stringValue)

        let followUp = ShortcutBridgeRequest(
            version: 1,
            requestID: "bridge-json-2",
            nonce: "bridge-json-nonce-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: digest,
            approvalToken: token,
            timeoutMS: 5_000)
        let followUpFrame = try JSONEncoder().encode(followUp)
        let resultData = try await Task.detached {
            try Self.sendBridgeFrame(followUpFrame, to: socketPath)
        }.value
        let result = try JSONDecoder().decode(ShortcutBridgeReply.self, from: resultData)
        XCTAssertEqual(result.status, "ok")
        XCTAssertEqual(result.requestID, followUp.requestID)
        XCTAssertEqual(result.result?.objectValue?["ok"], .bool(true))
        XCTAssertEqual(result.result?.objectValue?["count"], .number(2.0))
        XCTAssertNil(result.error)
    }

    func testTheShippedSkillStatesTheRegisteredBridgeContract() throws {
        let url = try XCTUnwrap(Bundle(for: ShortcutTests.self).url(
            forResource: "SKILL", withExtension: "md", subdirectory: "Skills/shortcuts"))
        let skill = try String(contentsOf: url, encoding: .utf8)
        XCTAssertTrue(url.path.hasSuffix("/Skills/shortcuts/SKILL.md"))
        XCTAssertTrue(skill.contains("capability is exactly `shortcuts.invoke`"))
        XCTAssertTrue(skill.contains("reason `shortcut_not_registered`"))
        XCTAssertTrue(skill.contains("Never claim that Leg can create, edit, enable"))
    }

    private nonisolated static func sendBridgeFrame(_ payload: Data,
                                                    to path: String) throws -> Data {
        var frame = payload
        frame.append(0x0A)

        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw NSError(domain: "ShortcutTests", code: Int(errno),
                          userInfo: [NSLocalizedDescriptionKey: "could not create a Unix socket"])
        }
        defer { Darwin.close(fd) }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        let pathCapacity = MemoryLayout.size(ofValue: address.sun_path)
        guard bytes.count + 1 <= pathCapacity else {
            throw NSError(domain: "ShortcutTests", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "the test socket name is too long"])
        }
        withUnsafeMutableBytes(of: &address.sun_path) { storage in
            for (index, byte) in bytes.enumerated() {
                storage[index] = byte
            }
            storage[bytes.count] = 0
        }
        let pathOffset = MemoryLayout<sockaddr_un>.offset(of: \.sun_path) ?? 0
        let length = socklen_t(pathOffset + bytes.count + 1)
        withUnsafeMutableBytes(of: &address) { storage in
            storage[0] = UInt8(length)
        }
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, length)
            }
        }
        guard connected == 0 else {
            throw NSError(domain: "ShortcutTests", code: Int(errno),
                          userInfo: [NSLocalizedDescriptionKey: "could not connect to the bridge server"])
        }

        let sent = frame.withUnsafeBytes { storage -> Int in
            guard let baseAddress = storage.baseAddress else { return -1 }
            return Darwin.send(fd, baseAddress, storage.count, 0)
        }
        guard sent == frame.count else {
            throw NSError(domain: "ShortcutTests", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "could not write the bridge request"])
        }

        var response = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while true {
            let count = buffer.withUnsafeMutableBytes { storage -> Int in
                guard let baseAddress = storage.baseAddress else { return -1 }
                return Darwin.recv(fd, baseAddress, storage.count, 0)
            }
            guard count > 0 else {
                throw NSError(domain: "ShortcutTests", code: 1,
                              userInfo: [NSLocalizedDescriptionKey: "the bridge server closed without a reply"])
            }
            response.append(buffer, count: count)
            if let newline = response.firstIndex(of: 0x0A) {
                return Data(response[..<newline])
            }
        }
    }
}
