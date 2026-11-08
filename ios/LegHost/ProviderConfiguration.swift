import Foundation
import Security

/// The provider a host configuration names.
///
/// The guest speaks the Anthropic Messages protocol. The two OpenAI wire
/// variants are named here so the host can refuse them with a stable
/// diagnostic instead of an opaque failure.
enum HostProvider: String, CaseIterable, Hashable {
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

/// The fields the setup form can report validation for.
enum ProviderConfigurationField: String, Hashable {
    case provider
    case apiKey = "api-key"
    case baseURL = "base-url"
    case model
    case contextWindowTokens = "context-window-tokens"
}

/// A provider configuration before it is validated and stored.
struct ProviderConfigurationDraft: Equatable {
    /// The provider selected by the setup form.
    var provider: HostProvider
    /// The credential entered by the user.
    var apiKey: String
    /// The base URL entered by the user.
    var baseURL: String
    /// The model entered by the user.
    var model: String
    /// The context window stays a string until validation can report a field error.
    var contextWindowTokens: String

    /// An empty first-run form with the supported provider selected.
    static let empty = ProviderConfigurationDraft(provider: .anthropic,
                                                  apiKey: "",
                                                  baseURL: "https://api.anthropic.com",
                                                  model: "",
                                                  contextWindowTokens: "200000")

    init(provider: HostProvider,
         apiKey: String,
         baseURL: String,
         model: String,
         contextWindowTokens: String) {
        self.provider = provider
        self.apiKey = apiKey
        self.baseURL = baseURL
        self.model = model
        self.contextWindowTokens = contextWindowTokens
    }

    /// A form pre-filled from a configuration already held by the host.
    init(configuration: ProviderConfiguration) {
        self.init(provider: configuration.provider,
                  apiKey: configuration.apiKey,
                  baseURL: configuration.baseURL,
                  model: configuration.model,
                  contextWindowTokens: String(configuration.contextWindowTokens))
    }
}

/// A field-level validation failure from the setup form.
enum ProviderConfigurationValidationError: LocalizedError, Equatable {
    /// The UI must not offer a provider protocol this host cannot speak.
    case unsupportedProvider
    /// The credential is empty after surrounding whitespace is removed.
    case emptyAPIKey
    /// The base URL is not an absolute HTTP(S) URL with a host.
    case invalidBaseURL
    /// The model is empty after surrounding whitespace is removed.
    case emptyModel
    /// The context window is not a positive integer.
    case invalidContextWindow

    /// The setup field that needs correction.
    var field: ProviderConfigurationField {
        switch self {
        case .unsupportedProvider: return .provider
        case .emptyAPIKey: return .apiKey
        case .invalidBaseURL: return .baseURL
        case .emptyModel: return .model
        case .invalidContextWindow: return .contextWindowTokens
        }
    }

    var errorDescription: String? {
        switch self {
        case .unsupportedProvider:
            return "Choose Anthropic Messages."
        case .emptyAPIKey:
            return "Enter an API key."
        case .invalidBaseURL:
            return "Enter an absolute http:// or https:// URL with a host."
        case .emptyModel:
            return "Enter a model name."
        case .invalidContextWindow:
            return "Enter a positive whole number of tokens."
        }
    }
}

/// A failure while writing or clearing the host-owned Keychain item.
enum ProviderConfigurationStoreError: LocalizedError, Equatable {
    /// The operation that failed in the Security framework.
    case keychain(operation: Operation, status: OSStatus)

    /// The Security operation that failed.
    enum Operation: String, Equatable {
        case add
        case update
        case delete
    }

    var errorDescription: String? {
        switch self {
        case .keychain(let operation, _):
            switch operation {
            case .add, .update:
                return "The provider configuration could not be saved. The previous configuration was kept."
            case .delete:
                return "The provider configuration could not be cleared."
            }
        }
    }
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
    /// The base URL a run uses when it does not talk to the fixture.
    ///
    /// It is a closed port on the loopback interface, so a run without the
    /// fixture stays on the device: a turn of such a run fails with a
    /// transport error instead of reaching a provider.
    static let overrideBaseURL = "http://127.0.0.1:1"

    /// A call that adds one generic-password item.
    typealias ItemAdder = (_ attributes: CFDictionary,
                           _ result: UnsafeMutablePointer<CFTypeRef?>?) -> OSStatus
    /// A call that updates one generic-password item.
    typealias ItemUpdater = (_ query: CFDictionary,
                             _ attributes: CFDictionary) -> OSStatus
    /// A call that deletes one generic-password item.
    typealias ItemDeleter = (_ query: CFDictionary) -> OSStatus

    /// Reads the configuration the host owns.
    ///
    /// The DEBUG provider override replaces the Keychain item for one run and
    /// never writes anything: the override lives in memory and dies with the
    /// process. The separate UI-test reset control may clear the item once;
    /// a build without the DEBUG controls reads the item of the Keychain.
    ///
    /// The call into the Security framework is a parameter, so the boundary
    /// can be exercised without an item in the Keychain of the machine that
    /// runs the test: the fixtures hand in a call that answers with an item
    /// of their own, and they read the query of the production read.
    static func load(providerOverride: HostProvider?,
                     hasNoProvider: Bool,
                     copyMatching: ItemCopier = copyKeychainItem) throws -> ProviderConfiguration {
        #if DEBUG
        if hasNoProvider {
            throw ProviderConfigurationError.missing
        }
        if let provider = providerOverride {
            guard provider == .anthropic else {
                throw ProviderConfigurationError.unsupported(provider: provider.rawValue)
            }
            return ProviderConfiguration(provider: provider,
                                         apiKey: fixtureCredential,
                                         baseURL: overrideBaseURL,
                                         model: fixtureModel,
                                         contextWindowTokens: fixtureContextWindowTokens)
        }
        #endif
        return try decode(try readKeychainItem(copyMatching: copyMatching))
    }

    /// The call into the Security framework that looks the item up.
    ///
    /// The production read asks the Security framework for the item; a test
    /// hands in a call of its own and reads the query it was given.
    typealias ItemCopier = (_ query: CFDictionary,
                            _ result: UnsafeMutablePointer<CFTypeRef?>) -> OSStatus

    /// The call of the Security framework that looks an item up.
    static let copyKeychainItem: ItemCopier = SecItemCopyMatching

    /// The call that adds an item to the Keychain.
    static let addKeychainItem: ItemAdder = SecItemAdd
    /// The call that updates an item in the Keychain.
    static let updateKeychainItem: ItemUpdater = SecItemUpdate
    /// The call that deletes an item from the Keychain.
    static let deleteKeychainItem: ItemDeleter = SecItemDelete

    /// Validates and stores a setup form.
    ///
    /// Encoding and validation happen before Security is called. When an item
    /// already exists, the add returns `errSecDuplicateItem` and the update
    /// replaces its data in place; a failed update therefore leaves the old
    /// item intact.
    @discardableResult
    static func save(_ draft: ProviderConfigurationDraft,
                     add: ItemAdder = addKeychainItem,
                     update: ItemUpdater = updateKeychainItem) throws -> ProviderConfiguration {
        let configuration = try validate(draft)
        let data = try encode(configuration)
        var result: CFTypeRef?
        let addStatus = add(addAttributes(data) as CFDictionary, &result)
        if addStatus == errSecSuccess {
            return configuration
        }
        guard addStatus == errSecDuplicateItem else {
            throw ProviderConfigurationStoreError.keychain(operation: .add, status: addStatus)
        }

        let updateStatus = update(itemQuery() as CFDictionary, updateAttributes(data) as CFDictionary)
        guard updateStatus == errSecSuccess else {
            throw ProviderConfigurationStoreError.keychain(operation: .update, status: updateStatus)
        }
        return configuration
    }

    /// Removes the saved provider item. Clearing an already empty store is a success.
    static func clear(delete: ItemDeleter = deleteKeychainItem) throws {
        let status = delete(itemQuery() as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw ProviderConfigurationStoreError.keychain(operation: .delete, status: status)
        }
    }

    /// Reads the configuration item from the Keychain.
    ///
    /// The item is found by its class, its service and its account. The
    /// accessibility of an item is not a searchable attribute, so it is a
    /// property of the item the writer stores and not of the search that
    /// finds it.
    static func readKeychainItem(copyMatching: ItemCopier = copyKeychainItem) throws -> Data {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = copyMatching(query as CFDictionary, &item)
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
        guard let provider = HostProvider(rawValue: stored.provider) else {
            throw ProviderConfigurationError.missing
        }
        guard provider == .anthropic else {
            throw ProviderConfigurationError.unsupported(provider: provider.rawValue)
        }
        do {
            return try validate(ProviderConfigurationDraft(
                provider: provider,
                apiKey: stored.apiKey,
                baseURL: stored.baseURL,
                model: stored.model,
                contextWindowTokens: String(stored.contextWindowTokens)))
        } catch {
            throw ProviderConfigurationError.missing
        }
    }

    /// Turns a form into the exact in-memory configuration the guest needs.
    static func validate(_ draft: ProviderConfigurationDraft) throws -> ProviderConfiguration {
        if let error = validationErrors(draft).first {
            throw error
        }
        let apiKey = draft.apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        let baseURL = draft.baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
        let model = draft.model.trimmingCharacters(in: .whitespacesAndNewlines)
        let contextWindow = draft.contextWindowTokens.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let tokens = Int(contextWindow), tokens > 0 else {
            throw ProviderConfigurationValidationError.invalidContextWindow
        }
        return ProviderConfiguration(provider: .anthropic,
                                     apiKey: apiKey,
                                     baseURL: baseURL,
                                     model: model,
                                     contextWindowTokens: tokens)
    }

    /// Returns every field that needs correction before a form can be saved.
    ///
    /// `validate` still throws the first error for callers that need a single
    /// failure, while the setup view can show guidance for all invalid fields
    /// in one submission.
    static func validationErrors(_ draft: ProviderConfigurationDraft)
        -> [ProviderConfigurationValidationError] {
        var errors: [ProviderConfigurationValidationError] = []
        guard draft.provider == .anthropic else {
            return [.unsupportedProvider]
        }
        let apiKey = draft.apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        let baseURL = draft.baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
        let model = draft.model.trimmingCharacters(in: .whitespacesAndNewlines)
        let contextWindow = draft.contextWindowTokens.trimmingCharacters(in: .whitespacesAndNewlines)
        if apiKey.isEmpty { errors.append(.emptyAPIKey) }
        if !isHTTPURL(baseURL) { errors.append(.invalidBaseURL) }
        if model.isEmpty { errors.append(.emptyModel) }
        if !(Int(contextWindow).map { $0 > 0 } ?? false) {
            errors.append(.invalidContextWindow)
        }
        return errors
    }

    /// Whether a URL is an absolute HTTP(S) endpoint with a host.
    private static func isHTTPURL(_ value: String) -> Bool {
        guard let url = URL(string: value),
              let scheme = url.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              let host = url.host, !host.isEmpty else {
            return false
        }
        return true
    }

    /// The query that identifies the one item the host owns.
    private static func itemQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    /// The attributes used for a new item.
    private static func addAttributes(_ data: Data) -> [String: Any] {
        var attributes = itemQuery()
        attributes[kSecValueData as String] = data
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        return attributes
    }

    /// The values changed when an existing item is replaced.
    private static func updateAttributes(_ data: Data) -> [String: Any] {
        [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
    }

    /// Encodes exactly the documented five keys.
    private static func encode(_ configuration: ProviderConfiguration) throws -> Data {
        try JSONEncoder().encode(StoredConfiguration(configuration: configuration))
    }

    /// The stored configuration, as it is written down in the Keychain item.
    private struct StoredConfiguration: Codable {
        let provider: String
        let apiKey: String
        let baseURL: String
        let model: String
        let contextWindowTokens: Int

        init(configuration: ProviderConfiguration) {
            provider = configuration.provider.rawValue
            apiKey = configuration.apiKey
            baseURL = configuration.baseURL
            model = configuration.model
            contextWindowTokens = configuration.contextWindowTokens
        }

        enum CodingKeys: String, CodingKey {
            case provider
            case apiKey = "api_key"
            case baseURL = "base_url"
            case model
            case contextWindowTokens = "context_window_tokens"
        }
    }
}
