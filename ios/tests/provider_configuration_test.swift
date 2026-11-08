import Foundation
import Security

/// Host side fixtures for the provider configuration of the LegHost host.
///
/// The fixtures cover the boundary the host reads its provider
/// configuration at: the call into the Security framework, the query that
/// call carries, the JSON of the item and the two diagnostics the host
/// reports there. The call into the Security framework is handed in, so the
/// fixtures need neither a Keychain nor a simulator.
@main
struct ProviderConfigurationFixtures {
    private var failures = 0
    private var checks = 0

    static func main() {
        var fixtures = ProviderConfigurationFixtures()
        fixtures.run()
        print("\(fixtures.checks) checks, \(fixtures.failures) failures")
        if fixtures.failures > 0 {
            exit(1)
        }
    }

    private mutating func run() {
        aValidItemIsRead()
        theQueryFindsTheItemOfTheProvider()
        aValidDraftIsSavedAsTheFiveKeyItem()
        anExistingItemIsUpdatedInPlace()
        anUpdateFailureLeavesTheExistingItemAlone()
        clearingUsesTheProviderQuery()
        invalidDraftsDoNotCallTheKeychain()
        anItemThatIsNotThereIsMissing()
        anItemWithoutDataIsMissing()
        anItemWithoutOneOfTheKeysIsMissing()
        anItemWithAnExtraKeyIsMissing()
        anItemWithoutAPositiveContextWindowIsMissing()
        anItemWithAnEmptyValueIsMissing()
        anItemWithAnUnknownProviderIsMissing()
        anItemThatIsNotJsonIsMissing()
        theOpenAIProvidersAreUnsupported()
        theDiagnosticsAreStable()
    }

    // MARK: - The fixtures

    /// A valid item is read and turned into a configuration.
    private mutating func aValidItemIsRead() {
        let configuration = try? ProviderConfigurationStore.load(
            providerOverride: nil,
            hasNoProvider: false,
            copyMatching: Self.copying(Self.validItem))
        check(configuration != nil, "the valid item was not read")
        guard let configuration else { return }
        check(configuration.provider == .anthropic,
              "the provider of the valid item is \(configuration.provider)")
        check(configuration.apiKey == "sk-ant-test",
              "the credential of the valid item is \(configuration.apiKey)")
        check(configuration.baseURL == "https://api.anthropic.com",
              "the base URL of the valid item is \(configuration.baseURL)")
        check(configuration.model == "claude-test",
              "the model of the valid item is \(configuration.model)")
        check(configuration.contextWindowTokens == 200_000,
              "the context window of the valid item is \(configuration.contextWindowTokens)")
    }

    /// The query of the read names the item the host owns.
    ///
    /// The item is a generic password of the service of the provider, under
    /// the account of the provider, and the read asks for its data.
    private mutating func theQueryFindsTheItemOfTheProvider() {
        var seen: [String: Any]?
        _ = try? ProviderConfigurationStore.load(
            providerOverride: nil,
            hasNoProvider: false,
            copyMatching: { query, _ in
                seen = query as? [String: Any]
                return errSecItemNotFound
            })
        guard let query = seen else {
            check(false, "the read did not ask the Security framework for anything")
            return
        }
        check(query[kSecClass as String] as? String == (kSecClassGenericPassword as String),
              "the class of the query is \(String(describing: query[kSecClass as String]))")
        check(query[kSecAttrService as String] as? String == ProviderConfigurationStore.service,
              "the service of the query is \(String(describing: query[kSecAttrService as String]))")
        check(query[kSecAttrAccount as String] as? String == ProviderConfigurationStore.account,
              "the account of the query is \(String(describing: query[kSecAttrAccount as String]))")
        check(query[kSecReturnData as String] as? Bool == true,
              "the query does not ask for the data of the item")
        check(query[kSecMatchLimit as String] as? String == (kSecMatchLimitOne as String),
              "the match limit of the query is \(String(describing: query[kSecMatchLimit as String]))")
    }

    /// A valid draft is encoded with exactly the documented item attributes.
    private mutating func aValidDraftIsSavedAsTheFiveKeyItem() {
        var attributes: [String: Any]?
        var updated = false
        let configuration = try? ProviderConfigurationStore.save(
            Self.validDraft,
            add: { item, _ in
                attributes = item as? [String: Any]
                return errSecSuccess
            },
            update: { _, _ in
                updated = true
                return errSecSuccess
            })
        check(configuration?.provider == .anthropic, "the saved provider is not Anthropic")
        check(!updated, "the new item was unexpectedly updated")
        guard let attributes else {
            check(false, "the save did not provide Keychain attributes")
            return
        }
        check(attributes[kSecClass as String] as? String == (kSecClassGenericPassword as String),
              "the saved item is not a generic password")
        check(attributes[kSecAttrService as String] as? String == ProviderConfigurationStore.service,
              "the saved item has the wrong service")
        check(attributes[kSecAttrAccount as String] as? String == ProviderConfigurationStore.account,
              "the saved item has the wrong account")
        check(attributes[kSecAttrAccessible as String] as? String
              == (kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as String),
              "the saved item has the wrong accessibility")
        guard let data = attributes[kSecValueData as String] as? Data,
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            check(false, "the saved item has no JSON data")
            return
        }
        check(Set(object.keys) == Set(ProviderConfigurationStore.keys),
              "the saved item does not hold exactly the five documented keys")
        check(object["provider"] as? String == Self.validDraft.provider.rawValue,
              "the saved item has the wrong provider value")
        check(object["api_key"] as? String == Self.validDraft.apiKey,
              "the saved item has the wrong API key value")
        check(object["base_url"] as? String == Self.validDraft.baseURL,
              "the saved item has the wrong base URL value")
        check(object["model"] as? String == Self.validDraft.model,
              "the saved item has the wrong model value")
        check((object["context_window_tokens"] as? NSNumber)?.intValue
              == Int(Self.validDraft.contextWindowTokens),
              "the saved item has the wrong context window value")
    }

    /// A duplicate add is followed by an in-place update with the same item identity.
    private mutating func anExistingItemIsUpdatedInPlace() {
        var updateQuery: [String: Any]?
        var updateAttributes: [String: Any]?
        let configuration = try? ProviderConfigurationStore.save(
            Self.validDraft,
            add: { _, _ in errSecDuplicateItem },
            update: { query, attributes in
                updateQuery = query as? [String: Any]
                updateAttributes = attributes as? [String: Any]
                return errSecSuccess
            })
        check(configuration != nil, "the existing item was not updated")
        check(updateQuery?[kSecClass as String] as? String == (kSecClassGenericPassword as String),
              "the update query has the wrong class")
        check(updateQuery?[kSecAttrService as String] as? String == ProviderConfigurationStore.service,
              "the update query has the wrong service")
        check(updateQuery?[kSecAttrAccount as String] as? String == ProviderConfigurationStore.account,
              "the update query has the wrong account")
        check(updateAttributes?[kSecAttrAccessible as String] as? String
              == (kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as String),
              "the update has the wrong accessibility")
        check(updateAttributes?[kSecValueData as String] is Data,
              "the update has no replacement data")
    }

    /// A failed update is reported while the existing item's bytes remain.
    private mutating func anUpdateFailureLeavesTheExistingItemAlone() {
        let previousBytes = Data(Self.validItem.utf8)
        var keychainBytes: Data? = previousBytes
        var attemptedBytes: Data?
        do {
            _ = try ProviderConfigurationStore.save(
                Self.validDraft,
                add: { attributes, _ in
                    // The injected Keychain already has an item, so add finds
                    // the existing identity and the store takes the update path.
                    guard keychainBytes == nil else { return errSecDuplicateItem }
                    keychainBytes = (attributes as? [String: Any])?[kSecValueData as String] as? Data
                    return keychainBytes == nil ? errSecParam : errSecSuccess
                },
                update: { _, attributes in
                    attemptedBytes = (attributes as? [String: Any])?[kSecValueData as String] as? Data
                    // Model a failed Security update: the old item is not
                    // replaced when the operation returns an error.
                    return errSecIO
                })
            check(false, "the failed update was accepted")
        } catch let error as ProviderConfigurationStoreError {
            check(error == .keychain(operation: .update, status: errSecIO),
                  "the failed update reported \(error)")
            check(error.localizedDescription
                  == "The provider configuration could not be saved. The previous configuration was kept.",
                  "the failed update exposed a non-generic message: \(error.localizedDescription)")
            check(!error.localizedDescription.contains("sk-ant-test"),
                  "the failed update exposed the previous credential")
        } catch {
            check(false, "the failed update reported the wrong error")
        }
        check(attemptedBytes != nil, "the failed update did not receive replacement bytes")
        if let attemptedBytes {
            check(attemptedBytes != previousBytes,
                  "the failed update did not attempt to replace the item")
        }
        check(keychainBytes == previousBytes,
              "the failed update changed the injected Keychain bytes")
        guard let keychainBytes else {
            check(false, "the previous item could not be read after the failed update")
            return
        }
        if let readBack = try? ProviderConfigurationStore.readKeychainItem(
            copyMatching: Self.copying(keychainBytes)) {
            check(readBack == previousBytes,
                  "the failed update did not leave the previous item readable")
        } else {
            check(false, "the previous item could not be read after the failed update")
        }
    }

    /// Clearing identifies the same service and account and does not read data.
    private mutating func clearingUsesTheProviderQuery() {
        var query: [String: Any]?
        do {
            try ProviderConfigurationStore.clear { item in
                query = item as? [String: Any]
                return errSecSuccess
            }
            check(query?[kSecClass as String] as? String == (kSecClassGenericPassword as String),
                  "the clear query has the wrong class")
            check(query?[kSecAttrService as String] as? String == ProviderConfigurationStore.service,
                  "the clear query has the wrong service")
            check(query?[kSecAttrAccount as String] as? String == ProviderConfigurationStore.account,
                  "the clear query has the wrong account")
        } catch {
            check(false, "the clear failed")
        }
    }

    /// Invalid fields are rejected before any Security operation is attempted.
    private mutating func invalidDraftsDoNotCallTheKeychain() {
        let cases: [(ProviderConfigurationDraft, ProviderConfigurationValidationError)] = [
            (Self.validDraft.with(apiKey: ""), .emptyAPIKey),
            (Self.validDraft.with(baseURL: "ftp://example.com"), .invalidBaseURL),
            (Self.validDraft.with(baseURL: "/relative"), .invalidBaseURL),
            (Self.validDraft.with(model: ""), .emptyModel),
            (Self.validDraft.with(contextWindowTokens: "0"), .invalidContextWindow),
            (Self.validDraft.with(contextWindowTokens: "not-a-number"), .invalidContextWindow),
            (Self.validDraft.with(provider: .openAIResponses), .unsupportedProvider),
        ]
        for (draft, expected) in cases {
            var calls = 0
            do {
                _ = try ProviderConfigurationStore.save(
                    draft,
                    add: { _, _ in calls += 1; return errSecSuccess },
                    update: { _, _ in calls += 1; return errSecSuccess })
                check(false, "the invalid draft was accepted")
            } catch let error as ProviderConfigurationValidationError {
                check(error == expected, "the invalid draft reported the wrong field")
            } catch {
                check(false, "the invalid draft reported the wrong error")
            }
            check(calls == 0, "the invalid draft called Security")
        }
    }

    /// There is no item, so the configuration is missing.
    private mutating func anItemThatIsNotThereIsMissing() {
        expect(.missing, "the item that is not there") {
            try ProviderConfigurationStore.load(providerOverride: nil,
                                                hasNoProvider: false,
                                                copyMatching: { _, _ in errSecItemNotFound })
        }
    }

    /// The read found no data, so the configuration is missing.
    private mutating func anItemWithoutDataIsMissing() {
        expect(.missing, "the item without data") {
            try ProviderConfigurationStore.load(providerOverride: nil,
                                                hasNoProvider: false,
                                                copyMatching: { _, _ in errSecSuccess })
        }
    }

    /// The item has to hold exactly the five keys.
    private mutating func anItemWithoutOneOfTheKeysIsMissing() {
        expect(.missing, "the item without a key") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                copyMatching: Self.copying(Self.itemWithoutKey))
        }
    }

    /// The item has to hold exactly the five keys.
    private mutating func anItemWithAnExtraKeyIsMissing() {
        expect(.missing, "the item with an extra key") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                copyMatching: Self.copying(Self.itemWithExtraKey))
        }
    }

    /// The context window has to be a positive number.
    private mutating func anItemWithoutAPositiveContextWindowIsMissing() {
        for window in ["0", "-1"] {
            expect(.missing, "the item with a context window of \(window)") {
                try ProviderConfigurationStore.load(
                    providerOverride: nil,
                    hasNoProvider: false,
                    copyMatching: Self.copying(Self.item(withContextWindow: window)))
            }
        }
    }

    /// The values of the item have to be usable.
    private mutating func anItemWithAnEmptyValueIsMissing() {
        for key in ["api_key", "base_url", "model"] {
            expect(.missing, "the item with an empty \(key)") {
                try ProviderConfigurationStore.load(
                    providerOverride: nil,
                    hasNoProvider: false,
                    copyMatching: Self.copying(Self.item(withEmptyValue: key)))
            }
        }
    }

    /// A provider the host does not speak is refused, not accepted.
    private mutating func anItemWithAnUnknownProviderIsMissing() {
        expect(.missing, "the item with an unknown provider") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                copyMatching: Self.copying(Self.item(withProvider: "some-other-provider")))
        }
    }

    /// An item that is not JSON is no configuration.
    private mutating func anItemThatIsNotJsonIsMissing() {
        expect(.missing, "the item that is not JSON") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                copyMatching: Self.copying("this is not JSON"))
        }
    }

    /// The two OpenAI wire variants are refused with their own diagnostic.
    private mutating func theOpenAIProvidersAreUnsupported() {
        for provider in ["openai-chat-completions", "openai-responses"] {
            expect(.unsupported(provider: provider), "the item of \(provider)") {
                try ProviderConfigurationStore.load(
                    providerOverride: nil,
                    hasNoProvider: false,
                    copyMatching: Self.copying(Self.item(withProvider: provider)))
            }
        }
    }

    /// The diagnostics the host shows are stable.
    private mutating func theDiagnosticsAreStable() {
        check(ProviderConfigurationError.missing.diagnostic == "IOS_PROVIDER_CONFIG_MISSING",
              "the diagnostic of a missing configuration is \(ProviderConfigurationError.missing.diagnostic)")
        check(ProviderConfigurationError.unsupported(provider: "anthropic").diagnostic
              == "IOS_PROVIDER_CONFIG_UNSUPPORTED",
              "the diagnostic of an unsupported configuration is "
              + "\(ProviderConfigurationError.unsupported(provider: "anthropic").diagnostic)")
    }

    // MARK: - The helpers

    /// Checks that the read reports the expected error.
    private mutating func expect(_ expected: ProviderConfigurationError,
                                 _ what: String,
                                 _ body: () throws -> ProviderConfiguration) {
        do {
            let configuration = try body()
            check(false, "\(what) was read as \(configuration)")
        } catch let error as ProviderConfigurationError {
            check(error == expected, "\(what) reported \(error), expected \(expected)")
        } catch {
            check(false, "\(what) reported \(error)")
        }
    }

    /// Records the outcome of one check.
    private mutating func check(_ condition: Bool, _ message: @autoclosure () -> String) {
        checks += 1
        if !condition {
            failures += 1
            FileHandle.standardError.write(Data("FAIL: \(message())\n".utf8))
        }
    }

    // MARK: - The items

    /// A call into the Security framework that answers with the item.
    private static func copying(_ item: String) -> ProviderConfigurationStore.ItemCopier {
        { _, result in
            result.pointee = (item.data(using: .utf8) ?? Data()) as NSData
            return errSecSuccess
        }
    }

    /// A call into Security that answers with existing item bytes.
    private static func copying(_ data: Data) -> ProviderConfigurationStore.ItemCopier {
        { _, result in
            result.pointee = data as NSData
            return errSecSuccess
        }
    }

    /// An item that holds a usable configuration.
    private static let validItem = """
    {"provider":"anthropic","api_key":"sk-ant-test","base_url":"https://api.anthropic.com",\
    "model":"claude-test","context_window_tokens":200000}
    """

    /// A usable form used by the write-side fixtures.
    private static let validDraft = ProviderConfigurationDraft(
        provider: .anthropic,
        apiKey: "sk-ant-test",
        baseURL: "https://api.anthropic.com",
        model: "claude-test",
        contextWindowTokens: "200000")

    /// An item that does not hold every key.
    private static let itemWithoutKey = """
    {"provider":"anthropic","api_key":"sk-ant-test","base_url":"https://api.anthropic.com",\
    "context_window_tokens":200000}
    """

    /// An item that holds more than the five keys.
    private static let itemWithExtraKey = """
    {"provider":"anthropic","api_key":"sk-ant-test","base_url":"https://api.anthropic.com",\
    "model":"claude-test","context_window_tokens":200000,"extra":"value"}
    """

    /// An item with a provider of its own.
    private static func item(withProvider provider: String) -> String {
        """
        {"provider":"\(provider)","api_key":"sk-ant-test","base_url":"https://api.anthropic.com",\
        "model":"claude-test","context_window_tokens":200000}
        """
    }

    /// An item with a context window of its own.
    private static func item(withContextWindow window: String) -> String {
        """
        {"provider":"anthropic","api_key":"sk-ant-test","base_url":"https://api.anthropic.com",\
        "model":"claude-test","context_window_tokens":\(window)}
        """
    }

    /// An item with an empty value of its own.
    private static func item(withEmptyValue key: String) -> String {
        var values = [
            "provider": "anthropic",
            "api_key": "sk-ant-test",
            "base_url": "https://api.anthropic.com",
            "model": "claude-test",
            "context_window_tokens": "200000",
        ]
        values[key] = ""
        let pairs = values.keys.sorted()
            .map { key in "\"\(key)\":\"\(values[key] ?? "")\"" }
            .joined(separator: ",")
        return "{\(pairs)}"
    }
}

private extension ProviderConfigurationDraft {
    /// Returns a copy with one field replaced for a validation fixture.
    func with(provider: HostProvider? = nil,
              apiKey: String? = nil,
              baseURL: String? = nil,
              model: String? = nil,
              contextWindowTokens: String? = nil) -> ProviderConfigurationDraft {
        ProviderConfigurationDraft(provider: provider ?? self.provider,
                                   apiKey: apiKey ?? self.apiKey,
                                   baseURL: baseURL ?? self.baseURL,
                                   model: model ?? self.model,
                                   contextWindowTokens: contextWindowTokens ?? self.contextWindowTokens)
    }
}
