import Foundation

/// Host side fixtures for the provider configuration of the LegHost host.
///
/// The fixtures cover the boundary the host reads its provider
/// configuration at: the read of the item of the Keychain, the JSON of the
/// item and the two diagnostics the host reports there. The read of the item
/// is handed in, so the fixtures need neither a Keychain nor a simulator.
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
        anItemThatIsNotThereIsMissing()
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
            reader: { Data(Self.validItem.utf8) })
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

    /// There is no item, so the configuration is missing.
    private mutating func anItemThatIsNotThereIsMissing() {
        expect(.missing, "the item that is not there") {
            try ProviderConfigurationStore.load(providerOverride: nil,
                                                hasNoProvider: false,
                                                reader: { throw ProviderConfigurationError.missing })
        }
    }

    /// The item has to hold exactly the five keys.
    private mutating func anItemWithoutOneOfTheKeysIsMissing() {
        expect(.missing, "the item without a key") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                reader: { Data(Self.itemWithoutKey.utf8) })
        }
    }

    /// The item has to hold exactly the five keys.
    private mutating func anItemWithAnExtraKeyIsMissing() {
        expect(.missing, "the item with an extra key") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                reader: { Data(Self.itemWithExtraKey.utf8) })
        }
    }

    /// The context window has to be a positive number.
    private mutating func anItemWithoutAPositiveContextWindowIsMissing() {
        for window in ["0", "-1"] {
            expect(.missing, "the item with a context window of \(window)") {
                try ProviderConfigurationStore.load(
                    providerOverride: nil,
                    hasNoProvider: false,
                    reader: { Data(Self.item(withContextWindow: window).utf8) })
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
                    reader: { Data(Self.item(withEmptyValue: key).utf8) })
            }
        }
    }

    /// A provider the host does not speak is refused, not accepted.
    private mutating func anItemWithAnUnknownProviderIsMissing() {
        expect(.missing, "the item with an unknown provider") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                reader: { Data(Self.item(withProvider: "some-other-provider").utf8) })
        }
    }

    /// An item that is not JSON is no configuration.
    private mutating func anItemThatIsNotJsonIsMissing() {
        expect(.missing, "the item that is not JSON") {
            try ProviderConfigurationStore.load(
                providerOverride: nil,
                hasNoProvider: false,
                reader: { Data("this is not JSON".utf8) })
        }
    }

    /// The two OpenAI wire variants are refused with their own diagnostic.
    private mutating func theOpenAIProvidersAreUnsupported() {
        for provider in ["openai-chat-completions", "openai-responses"] {
            expect(.unsupported(provider: provider), "the item of \(provider)") {
                try ProviderConfigurationStore.load(
                    providerOverride: nil,
                    hasNoProvider: false,
                    reader: { Data(Self.item(withProvider: provider).utf8) })
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

    /// An item that holds a usable configuration.
    private static let validItem = """
    {"provider":"anthropic","api_key":"sk-ant-test","base_url":"https://api.anthropic.com",\
    "model":"claude-test","context_window_tokens":200000}
    """

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
