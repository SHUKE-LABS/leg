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
        XCTAssertEqual(components.host, "run-shortcut")
        XCTAssertEqual(query["name"], "Morning report")
        XCTAssertEqual(query["input"], "{\"format\":\"json\"}")
        XCTAssertEqual(query["x-success"], "legshortcuts://success?request_id=shortcut-test-2")
        XCTAssertEqual(query["x-cancel"], "legshortcuts://cancel?request_id=shortcut-test-2")
        XCTAssertEqual(query["x-error"], "legshortcuts://error?request_id=shortcut-test-2")
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
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 10_000_000)
                    adapter.handleCallback(URL(string:
                        "legshortcuts://success?request_id=shortcut-test-3&result=%7B%22ok%22%3Atrue%7D")!)
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
                                     resolvingAgainstBaseURL: false)?.host, "run-shortcut")
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
