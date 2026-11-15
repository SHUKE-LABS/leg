import XCTest
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
            requestID: "shortcut-test-2"))
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
        XCTAssertEqual(query["x-success"], "legshortcuts://success?request_id=shortcut-test-2")
        XCTAssertEqual(query["x-cancel"], "legshortcuts://cancel?request_id=shortcut-test-2")
        XCTAssertEqual(query["x-error"], "legshortcuts://error?request_id=shortcut-test-2")
    }

    func testShortcutURLPassesAUserSelectedFileThroughThePublicFileParameter() throws {
        let file = URL(fileURLWithPath: "/private/var/mobile/Containers/Shared/AppGroup/report.txt")
        let url = try XCTUnwrap(ShortcutsAdapter.shortcutURL(
            name: "Import report", file: file, requestID: "shortcut-file-1"))
        let components = try XCTUnwrap(URLComponents(url: url, resolvingAgainstBaseURL: false))
        let query = Dictionary(uniqueKeysWithValues: (components.queryItems ?? []).map {
            ($0.name, $0.value ?? "")
        })

        XCTAssertEqual(components.host, "x-callback-url")
        XCTAssertEqual(components.path, "/run-shortcut")
        XCTAssertEqual(query["input"], "file")
        XCTAssertEqual(query["file"], file.absoluteString)
    }

    func testFileInputDispatchesTheSelectedFileAndLeavesTheSourceToTheUser() async throws {
        let suite = "LegHost.ShortcutTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let source = FileManager.default.temporaryDirectory
            .appendingPathComponent("shortcut-\(UUID().uuidString).txt")
        try Data("selected by the user".utf8).write(to: source)
        defer { try? FileManager.default.removeItem(at: source) }

        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: "Import file", inputKind: .file)
        var adapter: ShortcutsAdapter!
        var openedURL: URL?
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                openedURL = url
                Task { @MainActor in
                    adapter.handleCallback(URL(string:
                        "legshortcuts://success?request_id=shortcut-file-dispatch-2&result=done")!)
                }
                return true
            },
            filePicker: { source })
        let input: [String: BridgeJSONValue] = [
            "name": .string("Import file"),
            "input": .object([:]),
        ]
        let first = ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-file-dispatch-1",
            nonce: "nonce-file-dispatch-1",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: String(repeating: "3", count: 64),
            approvalToken: nil,
            timeoutMS: 5_000)
        let approval = await adapter.handle(first)
        let token = try XCTUnwrap(approval.result?.objectValue?["approval_token"]?.stringValue)
        let result = await adapter.handle(ShortcutBridgeRequest(
            version: 1,
            requestID: "shortcut-file-dispatch-2",
            nonce: "nonce-file-dispatch-2",
            capability: ShortcutsAdapter.capability,
            input: input,
            inputSHA256: first.inputSHA256,
            approvalToken: token,
            timeoutMS: 5_000))

        XCTAssertEqual(result.status, "ok")
        let components = try XCTUnwrap(URLComponents(url: try XCTUnwrap(openedURL),
                                                     resolvingAgainstBaseURL: false))
        let query = Dictionary(uniqueKeysWithValues: (components.queryItems ?? []).map {
            ($0.name, $0.value ?? "")
        })
        XCTAssertEqual(query["input"], "file")
        XCTAssertEqual(query["file"], source.absoluteString)
        XCTAssertTrue(FileManager.default.fileExists(atPath: source.path))
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
                      let requestID = URLComponents(url: callbackURL,
                                                   resolvingAgainstBaseURL: false)?
                        .queryItems?.first(where: { $0.name == "request_id" })?.value else {
                    return false
                }
                let callback = "legshortcuts://error?request_id=\(requestID)&errorMessage=The%20shortcut%20could%20not%20be%20found"
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
                      let requestID = URLComponents(url: callbackURL,
                                                   resolvingAgainstBaseURL: false)?
                        .queryItems?.first(where: { $0.name == "request_id" })?.value else {
                    return false
                }
                Task { @MainActor in
                    adapter.handleCallback(URL(string:
                        "legshortcuts://success?request_id=\(requestID)&result=%7B%22ok%22%3Atrue%7D")!)
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
        func requestID(from url: URL, callbackName: String) -> String? {
            guard let callbackString = URLComponents(url: url,
                                                     resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == callbackName })?.value,
                  let callbackURL = URL(string: callbackString) else { return nil }
            return URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "request_id" })?.value
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
                guard let requestID = requestID(from: url, callbackName: "x-success") else {
                    return false
                }
                Task { @MainActor in
                    missingResultAdapter.handleCallback(
                        URL(string: "legshortcuts://success?request_id=\(requestID)")!)
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
                guard let requestID = requestID(from: url, callbackName: "x-cancel") else {
                    return false
                }
                Task { @MainActor in
                    cancelledAdapter.handleCallback(
                        URL(string: "legshortcuts://cancel?request_id=\(requestID)")!)
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

    func testTheShippedSkillStatesTheRegisteredBridgeContract() throws {
        let url = try XCTUnwrap(Bundle(for: ShortcutTests.self).url(
            forResource: "SKILL", withExtension: "md", subdirectory: "Skills/shortcuts"))
        let skill = try String(contentsOf: url, encoding: .utf8)
        XCTAssertTrue(url.path.hasSuffix("/Skills/shortcuts/SKILL.md"))
        XCTAssertTrue(skill.contains("capability is exactly `shortcuts.invoke`"))
        XCTAssertTrue(skill.contains("reason `shortcut_not_registered`"))
        XCTAssertTrue(skill.contains("Never claim that Leg can create, edit, enable"))
    }
}
