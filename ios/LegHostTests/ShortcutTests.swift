import XCTest
@testable import LegHost

/// Focused tests for the host-owned Shortcut registration and the unified
/// `BridgeCapabilityAdapter` implementation.
@MainActor
final class ShortcutTests: XCTestCase {
    func testRegistrationsUseExactNamesAndPersistTheirInputKind() throws {
        let defaults = try suiteDefaults()
        defer { defaults.removePersistentDomain(forName: defaultsSuite) }

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
        let defaults = try suiteDefaults()
        defer { defaults.removePersistentDomain(forName: defaultsSuite) }
        let key = "leg.shortcuts.registrations.v1"
        let malformed = Data("not-json".utf8)
        defaults.set(malformed, forKey: key)

        let store = ShortcutRegistrationStore(defaults: defaults)

        XCTAssertEqual(store.loadError, .invalidStore)
        XCTAssertTrue(store.registrations.isEmpty)
        XCTAssertThrowsError(try store.save(name: "Lost registration", inputKind: .text)) { error in
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

    func testUnregisteredNameIsDeniedBeforeConfirmationOrURLDispatch() async throws {
        let (store, _) = try makeStore()
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

        do {
            _ = try await adapter.preview(request(name: "Not registered",
                                                   input: .object(["text": .string("hello")])),
                                          context: context())
            XCTFail("an unregistered Shortcut was previewed")
        } catch let error as BridgeFailure {
            XCTAssertEqual(error, .denied("shortcut_not_registered",
                                         "the Shortcut name is not registered in LegHost"))
        }
        XCTAssertFalse(confirmationCalled)
        XCTAssertFalse(openerCalled)
    }

    func testPreviewIsTheOnlyConfirmationAndPerformUsesTheSameListenerContract() async throws {
        let (store, _) = try makeStore(name: "JSON result", kind: .json)
        var confirmationCount = 0
        var adapter: ShortcutsAdapter!
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in
                confirmationCount += 1
                return true
            },
            opener: { url in
                guard let state = Self.callbackState(in: url, callbackName: "x-success") else {
                    return false
                }
                Task { @MainActor in
                    adapter.handleCallback(URL(string:
                        "legshortcuts://success?state=\(state)&result=%7B%22ok%22%3Atrue%7D")!)
                }
                return true
            })

        let typed = request(name: "JSON result",
                            input: .object(["format": .string("json"),
                                            "value": .string("deterministic")]))
        let review = try await adapter.preview(typed, context: context())
        XCTAssertEqual(review["shortcut"]?.stringValue, "JSON result")
        XCTAssertEqual(review["input"]?.stringValue, "JSON object with fields: format, value")
        XCTAssertEqual(confirmationCount, 1)

        let result = try await adapter.perform(typed, context: context())
        XCTAssertEqual(result, .object(["ok": .bool(true)]))
        XCTAssertEqual(confirmationCount, 1, "the listener's preview is the only confirmation")
        let _: BridgeCapabilityAdapter = adapter
    }

    func testTextAndJSONInputKindsAreCheckedByTheNativeAdapter() async throws {
        let (store, _) = try makeStore(name: "Text shortcut", kind: .text)
        let adapter = ShortcutsAdapter(registry: store, confirmation: { _, _ in true })

        let textReview = try await adapter.preview(
            request(name: "Text shortcut", input: .object(["text": .string("hello")])),
            context: context())
        XCTAssertEqual(textReview["input"]?.stringValue, "text input (5 characters)")

        do {
            _ = try await adapter.preview(
                request(name: "Text shortcut", input: .object(["format": .string("json")])),
                context: context())
            XCTFail("JSON-shaped input was accepted by a text Shortcut")
        } catch let error as BridgeFailure {
            XCTAssertEqual(error.reason, "invalid_input")
        }
    }

    func testUnknownAndStaleCallbackStatesCannotSettleARequest() async throws {
        let (store, _) = try makeStore(name: "Callback state", kind: .text)
        var adapter: ShortcutsAdapter!
        var openedURLs: [URL] = []
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                openedURLs.append(url)
                return true
            })

        let request = self.request(name: "Callback state",
                                   input: .object(["text": .string("hello")]))
        let task = Task { @MainActor in
            try await adapter.perform(request, context: context(timeout: 0.2))
        }
        while openedURLs.isEmpty {
            await Task.yield()
        }
        adapter.handleCallback(URL(string: "legshortcuts://success?state=forged&result=forged")!)
        let state = try XCTUnwrap(Self.callbackState(in: openedURLs[0], callbackName: "x-success"))
        adapter.handleCallback(URL(string: "legshortcuts://success?state=\(state)&result=done")!)
        let result = try await task.value
        XCTAssertEqual(result, .object(["kind": .string("text"), "value": .string("done")]))
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

    func testCallbackFailuresMapToStableBridgeFailures() async throws {
        let (store, _) = try makeStore(name: "Missing result", kind: .text)
        var adapter: ShortcutsAdapter!
        adapter = ShortcutsAdapter(
            registry: store,
            confirmation: { _, _ in true },
            opener: { url in
                guard let state = Self.callbackState(in: url, callbackName: "x-success") else {
                    return false
                }
                Task { @MainActor in
                    adapter.handleCallback(URL(string: "legshortcuts://success?state=\(state)")!)
                }
                return true
            })

        do {
            _ = try await adapter.perform(
                request(name: "Missing result", input: .object(["text": .string("hello")])),
                context: context())
            XCTFail("a missing callback result was accepted")
        } catch let error as BridgeFailure {
            XCTAssertEqual(error.reason, "missing_result")
        }
    }

    // MARK: Helpers

    private let defaultsSuite = "LegHost.ShortcutTests.\(UUID().uuidString)"

    private func suiteDefaults() throws -> UserDefaults {
        try XCTUnwrap(UserDefaults(suiteName: defaultsSuite))
    }

    private func makeStore(name: String = "Shortcut", kind: ShortcutInputKind = .json)
        throws -> (ShortcutRegistrationStore, UserDefaults) {
        let defaults = try suiteDefaults()
        defaults.removePersistentDomain(forName: defaultsSuite)
        let store = ShortcutRegistrationStore(defaults: defaults)
        try store.save(name: name, inputKind: kind)
        return (store, defaults)
    }

    private func request(name: String, input: BridgeJSON) -> BridgeCapabilityRequest {
        .shortcutsInvoke(ShortcutsInvokeInput(name: name, input: input))
    }

    private func context(timeout: TimeInterval = 5) -> BridgeOperationContext {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("shortcut-test-\(UUID().uuidString)", isDirectory: true)
        return BridgeOperationContext(
            generation: "test-generation",
            operationID: "test-operation",
            deadline: Date().addingTimeInterval(timeout),
            paths: BridgeOperationPaths(staging: root, managedRoot: root))
    }

    private static func callbackState(in url: URL, callbackName: String) -> String? {
        guard let callbackString = URLComponents(url: url,
                                                 resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == callbackName })?.value,
              let callbackURL = URL(string: callbackString) else { return nil }
        return URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == "state" })?.value
    }
}
