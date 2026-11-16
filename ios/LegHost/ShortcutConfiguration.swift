import Foundation
import Combine

/// The kinds of input a registered Shortcut is allowed to receive.
enum ShortcutInputKind: String, CaseIterable, Codable, Identifiable {
    case text
    case json

    var id: String { rawValue }

    var title: String {
        switch self {
        case .text: return "Text"
        case .json: return "JSON"
        }
    }

    var explanation: String {
        switch self {
        case .text: return "A bounded text value supplied by the guest."
        case .json: return "A bounded JSON object supplied by the guest."
        }
    }
}

/// One exact Shortcut registration. It contains no URL, source, or action
/// description: the host can invoke only this name through Apple's public URL.
struct ShortcutRegistration: Codable, Equatable, Identifiable {
    let name: String
    let inputKind: ShortcutInputKind

    var id: String { name }
}

/// Errors shown by the host-owned Shortcut registration form.
enum ShortcutRegistrationError: LocalizedError, Equatable {
    case emptyName
    case nameTooLong
    case controlCharacter
    case duplicateName
    case invalidStore

    var errorDescription: String? {
        switch self {
        case .emptyName:
            return "Enter the exact name of an existing Shortcut."
        case .nameTooLong:
            return "The Shortcut name is too long. Keep it to 256 UTF-8 bytes."
        case .controlCharacter:
            return "The Shortcut name cannot contain control characters."
        case .duplicateName:
            return "That Shortcut name is already registered."
        case .invalidStore:
            return "The saved Shortcut registrations could not be read."
        }
    }
}
/// The host-owned store for Shortcut registrations.
///
/// Registration metadata is not a credential. It is kept in UserDefaults so
/// it survives relaunch without putting any Shortcut input or source in the
/// guest root, the bridge ledger, or the exchange trail.
@MainActor
final class ShortcutRegistrationStore: ObservableObject {
    static let maximumNameBytes = 256
    private static let defaultsKey = "leg.shortcuts.registrations.v1"

    @Published private(set) var registrations: [ShortcutRegistration]
    @Published private(set) var loadError: ShortcutRegistrationError?
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        do {
            self.registrations = try Self.load(defaults: defaults)
            self.loadError = nil
        } catch {
            // Keep the invalid value intact and make the setup view read-only.
            // Silently replacing it with an empty list would discard the
            // user's registrations on the next save.
            self.registrations = []
            self.loadError = .invalidStore
        }
    }

    /// Validates one name without consulting persistent state.
    nonisolated static func validateName(_ name: String) throws {
        guard !name.isEmpty else { throw ShortcutRegistrationError.emptyName }
        guard name.utf8.count <= maximumNameBytes else {
            throw ShortcutRegistrationError.nameTooLong
        }
        guard !name.unicodeScalars.contains(where: {
            $0.properties.generalCategory == .control
        }) else {
            throw ShortcutRegistrationError.controlCharacter
        }
    }

    /// Reads and validates the persisted registration list.
    nonisolated static func load(defaults: UserDefaults = .standard) throws -> [ShortcutRegistration] {
        guard let data = defaults.data(forKey: defaultsKey) else { return [] }
        let decoder = JSONDecoder()
        guard let registrations = try? decoder.decode([ShortcutRegistration].self, from: data),
              registrations.count <= 64,
              Set(registrations.map(\.name)).count == registrations.count
        else {
            throw ShortcutRegistrationError.invalidStore
        }
        for registration in registrations {
            try validateName(registration.name)
        }
        return registrations.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    /// Saves one registration. A name is explicit and exact, so changing its
    /// declared input kind requires removing it and registering it again.
    func save(name: String, inputKind: ShortcutInputKind) throws {
        guard loadError == nil else { throw ShortcutRegistrationError.invalidStore }
        try Self.validateName(name)
        guard !registrations.contains(where: { $0.name == name }) else {
            throw ShortcutRegistrationError.duplicateName
        }
        registrations.append(ShortcutRegistration(name: name, inputKind: inputKind))
        try persist()
        registrations = try Self.load(defaults: defaults)
    }

    /// Removes one exact registration.
    func remove(name: String) throws {
        guard loadError == nil else { throw ShortcutRegistrationError.invalidStore }
        registrations.removeAll { $0.name == name }
        try persist()
    }

    /// Finds only an exact, case-sensitive name.
    func registration(named name: String) -> ShortcutRegistration? {
        registrations.first { $0.name == name }
    }

    private func persist() throws {
        do {
            let data = try JSONEncoder().encode(registrations)
            defaults.set(data, forKey: Self.defaultsKey)
        } catch {
            throw ShortcutRegistrationError.invalidStore
        }
    }
}
// End of file
