import Foundation
import Security

/// The provider a host configuration names.
///
/// The guest speaks the Anthropic Messages protocol. The two OpenAI wire
/// variants are named here so the host can refuse them with a stable
/// diagnostic instead of an opaque failure.
enum HostProvider: String {
    case anthropic
    case openAIChatCompletions = "openai-chat-completions"
    case openAIResponses = "openai-responses"
}

/// The provider configuration the host owns for one guest session.
///
/// The host reads the configuration once, before it provisions the guest, and
/// hands the guest exactly the values it needs. Nothing else of the host
/// environment reaches the guest.
struct ProviderConfiguration: Equatable {
    /// The configured provider.
    let provider: HostProvider
    /// The provider credential. It reaches the guest only as the
    /// `ANTHROPIC_API_KEY` of the session environment.
    let apiKey: String
    /// The provider base URL.
    let baseURL: String
    /// The model the guest requests.
    let model: String
    /// The context window of the model, in tokens.
    let contextWindowTokens: Int
}

/// Why the host could not use a provider configuration.
///
/// The two cases are separate because they ask different things of the user:
/// a missing configuration has to be added, an unsupported one has to be
/// replaced.
enum ProviderConfigurationError: LocalizedError, Equatable {
    /// There is no usable configuration.
    case missing
    /// The configuration names a provider this host does not support.
    case unsupported(provider: String)

    /// The stable diagnostic the host shows and the tests assert on.
    var diagnostic: String {
        switch self {
        case .missing:
            return "IOS_PROVIDER_CONFIG_MISSING"
        case .unsupported:
            return "IOS_PROVIDER_CONFIG_UNSUPPORTED"
        }
    }

    var errorDescription: String? {
        switch self {
        case .missing:
            return "\(diagnostic): no usable provider configuration. Add a Keychain generic "
                + "password with the service \(ProviderConfigurationStore.service), the account "
                + "\(ProviderConfigurationStore.account) and the JSON keys "
                + "\(ProviderConfigurationStore.keys.joined(separator: ", "))."
        case .unsupported(let provider):
            return "\(diagnostic): the provider \(provider) is not supported. This host runs the "
                + "guest against the Anthropic Messages protocol."
        }
    }
}

/// The Keychain item the provider configuration lives in.
///
/// The item is one generic password. Its data is the UTF-8 JSON of the
/// configuration and it holds exactly the five documented keys, so a stale or
/// half-written item is refused instead of half-applied.
enum ProviderConfigurationStore {
    /// The Keychain service of the provider configuration.
    static let service = "com.shukelabs.leg.ioshost.provider"
    /// The Keychain account of the provider configuration.
    static let account = "default"
    /// The keys the configuration JSON holds, and nothing else.
    static let keys = ["provider", "api_key", "base_url", "model", "context_window_tokens"]

    /// The credential a fixture run uses. It is a sentinel, not a secret.
    static let fixtureCredential = "IOS_FIXTURE_CREDENTIAL_SENTINEL"
    /// The model a fixture run requests.
    static let fixtureModel = "claude-ios-fixture"
    /// The context window a fixture run uses.
    static let fixtureContextWindowTokens = 200_000

    /// Reads the configuration the host owns.
    ///
    /// The DEBUG launch controls replace the Keychain item for one run. They
    /// never write anything: the override lives in memory and dies with the
    /// process.
    static func load(options: HostLaunchOptions) throws -> ProviderConfiguration {
        #if DEBUG
        if options.hasNoProvider {
            throw ProviderConfigurationError.missing
        }
        if let provider = options.provider {
            guard provider == .anthropic else {
                throw ProviderConfigurationError.unsupported(provider: provider.rawValue)
            }
            return ProviderConfiguration(provider: provider,
                                         apiKey: fixtureCredential,
                                         baseURL: "",
                                         model: fixtureModel,
                                         contextWindowTokens: fixtureContextWindowTokens)
        }
        #endif
        return try decode(try readItem())
    }

    /// Reads the configuration item from the Keychain.
    static func readItem() throws -> Data {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess, let data = item as? Data else {
            throw ProviderConfigurationError.missing
        }
        return data
    }

    /// Turns the stored JSON into a configuration.
    static func decode(_ data: Data) throws -> ProviderConfiguration {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              Set(object.keys) == Set(keys)
        else {
            throw ProviderConfigurationError.missing
        }
        let stored: StoredConfiguration
        do {
            stored = try JSONDecoder().decode(StoredConfiguration.self, from: data)
        } catch {
            throw ProviderConfigurationError.missing
        }
        guard let provider = HostProvider(rawValue: stored.provider),
              !stored.apiKey.isEmpty,
              !stored.baseURL.isEmpty,
              !stored.model.isEmpty,
              stored.contextWindowTokens > 0
        else {
            throw ProviderConfigurationError.missing
        }
        guard provider == .anthropic else {
            throw ProviderConfigurationError.unsupported(provider: provider.rawValue)
        }
        return ProviderConfiguration(provider: provider,
                                     apiKey: stored.apiKey,
                                     baseURL: stored.baseURL,
                                     model: stored.model,
                                     contextWindowTokens: stored.contextWindowTokens)
    }

    /// The stored configuration, as it is written down in the Keychain item.
    private struct StoredConfiguration: Decodable {
        let provider: String
        let apiKey: String
        let baseURL: String
        let model: String
        let contextWindowTokens: Int

        enum CodingKeys: String, CodingKey {
            case provider
            case apiKey = "api_key"
            case baseURL = "base_url"
            case model
            case contextWindowTokens = "context_window_tokens"
        }
    }
}
