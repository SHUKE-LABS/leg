import Foundation

/// One JSON value, kept in the order the bytes carried it.
///
/// The bridge never reads a request through `JSONSerialization`: a dictionary
/// keeps the last of two members with the same name, so a request that names
/// a field twice would be read one way here and another way in the guest. The
/// value is materialized by `BridgeCanonicalJSON.parse` instead, which refuses
/// a duplicate name before any value exists.
indirect enum BridgeJSON: Equatable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([BridgeJSON])
    case object([BridgeJSONMember])

    /// The member called `key`, when this is an object that has one.
    subscript(key: String) -> BridgeJSON? {
        guard case .object(let members) = self else { return nil }
        return members.first { $0.key == key }?.value
    }

    /// The member names, when this is an object.
    var keys: [String]? {
        guard case .object(let members) = self else { return nil }
        return members.map(\.key)
    }

    var stringValue: String? {
        guard case .string(let text) = self else { return nil }
        return text
    }

    var boolValue: Bool? {
        guard case .bool(let flag) = self else { return nil }
        return flag
    }

    /// The value as an integer, when it is a number without a fraction that
    /// a binary64 holds exactly.
    var integerValue: Int64? {
        guard case .number(let number) = self, number.rounded() == number,
              abs(number) <= 9_007_199_254_740_992 else { return nil }
        return Int64(number)
    }

    var isNull: Bool { self == .null }

    /// An object with the members in the order they are given.
    static func object(_ members: KeyValuePairs<String, BridgeJSON>) -> BridgeJSON {
        .object(members.map { BridgeJSONMember(key: $0.key, value: $0.value) })
    }

    /// A number made of an integer.
    static func integer<T: BinaryInteger>(_ value: T) -> BridgeJSON {
        .number(Double(value))
    }
}

/// One member of a JSON object.
struct BridgeJSONMember: Equatable {
    let key: String
    let value: BridgeJSON
}

/// Why bytes are not a JSON document the bridge accepts.
enum BridgeCanonicalJSONError: Error, Equatable {
    /// The bytes are not JSON.
    case malformed(String)
    /// An object names the same member twice, after the escapes are undone.
    case duplicateKey(String)
    /// A number is not a finite binary64 value, or does not survive the
    /// conversion to one.
    case invalidNumber(String)
    /// The document nests deeper than the limit.
    case tooDeep
}

/// The RFC 8785 JSON Canonicalization Scheme the approval digest is made of.
///
/// The guest bridge and the host compute `input_sha256` over the canonical
/// bytes of the request input, so a reordered input continues an approval
/// and a changed value does not. Both sides read the raw token stream:
///
/// - a member name that occurs twice in one object, also after escapes are
///   undone, is refused;
/// - a number is read as an IEEE-754 binary64; a number that overflows, that
///   underflows to zero from a non-zero literal, or that is not finite is
///   refused (I-JSON);
/// - object members are sorted by the UTF-16 code units of their names;
/// - strings use the RFC 8785 escapes, and numbers the ECMAScript shortest
///   round-trip form.
enum BridgeCanonicalJSON {
    /// The deepest nesting the parser follows.
    static let maxDepth = 64

    /// Reads one JSON document out of `data`.
    ///
    /// A `lenient` read keeps duplicate names and reads a number that is not
    /// I-JSON as null; the listener uses it only to find the identity of a
    /// request the strict read refused, so the refusal can name it.
    static func parse(_ data: Data, lenient: Bool = false) throws -> BridgeJSON {
        var parser = Parser(bytes: [UInt8](data), lenient: lenient)
        parser.skipWhitespace()
        let value = try parser.value(depth: 0)
        parser.skipWhitespace()
        guard parser.atEnd else {
            throw BridgeCanonicalJSONError.malformed("trailing bytes after the document")
        }
        return value
    }

    /// The canonical bytes of a value.
    static func encode(_ value: BridgeJSON) -> Data {
        var out = ""
        write(value, into: &out)
        return Data(out.utf8)
    }

    /// The canonical bytes of the JSON document in `data`.
    static func canonicalize(_ data: Data) throws -> Data {
        encode(try parse(data))
    }

    /// The lowercase hex SHA-256 of the canonical bytes of a value.
    static func digest(_ value: BridgeJSON) -> String {
        sha256Hex(encode(value))
    }

    /// The lowercase hex SHA-256 of `data`.
    static func sha256Hex(_ data: Data) -> String {
        var context = sha256_ctx()
        sha256_init(&context)
        data.withUnsafeBytes { raw in
            if let base = raw.baseAddress {
                sha256_update(&context, base, raw.count)
            }
        }
        var digest = [UInt8](repeating: 0, count: 32)
        sha256_final(&context, &digest)
        return digest.map { String(format: "%02x", $0) }.joined()
    }

    // MARK: - Writing

    private static func write(_ value: BridgeJSON, into out: inout String) {
        switch value {
        case .null:
            out += "null"
        case .bool(let flag):
            out += flag ? "true" : "false"
        case .number(let number):
            out += formatNumber(number)
        case .string(let text):
            writeString(text, into: &out)
        case .array(let items):
            out += "["
            for (index, item) in items.enumerated() {
                if index > 0 { out += "," }
                write(item, into: &out)
            }
            out += "]"
        case .object(let members):
            out += "{"
            let sorted = members.sorted { utf16Less($0.key, $1.key) }
            for (index, member) in sorted.enumerated() {
                if index > 0 { out += "," }
                writeString(member.key, into: &out)
                out += ":"
                write(member.value, into: &out)
            }
            out += "}"
        }
    }

    /// Whether `left` sorts before `right` by UTF-16 code units.
    private static func utf16Less(_ left: String, _ right: String) -> Bool {
        left.utf16.lexicographicallyPrecedes(right.utf16)
    }

    private static func writeString(_ text: String, into out: inout String) {
        out += "\""
        for scalar in text.unicodeScalars {
            switch scalar {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\u{08}": out += "\\b"
            case "\u{09}": out += "\\t"
            case "\u{0A}": out += "\\n"
            case "\u{0C}": out += "\\f"
            case "\u{0D}": out += "\\r"
            default:
                if scalar.value < 0x20 {
                    out += String(format: "\\u%04x", scalar.value)
                } else {
                    out.unicodeScalars.append(scalar)
                }
            }
        }
        out += "\""
    }

    /// The ECMAScript `Number.prototype.toString` form of a finite number.
    ///
    /// Swift prints the shortest digits that round-trip, as ECMAScript does;
    /// only the layout differs, so the digits and the decimal exponent are
    /// taken out of Swift's text and laid out again.
    static func formatNumber(_ number: Double) -> String {
        if number == 0 { return "0" }
        let negative = number < 0
        var text = Substring(abs(number).description)
        var exponent = 0
        if let marker = text.firstIndex(where: { $0 == "e" || $0 == "E" }) {
            exponent = Int(text[text.index(after: marker)...]) ?? 0
            text = text[..<marker]
        }
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        let integerPart = parts[0]
        let fractionPart = parts.count > 1 ? parts[1] : ""
        var digits = Array(integerPart + fractionPart)
        // The decimal point sits after `point` digits.
        var point = integerPart.count + exponent
        while let first = digits.first, first == "0" {
            digits.removeFirst()
            point -= 1
        }
        while let last = digits.last, last == "0" {
            digits.removeLast()
        }
        let k = digits.count
        let n = point
        let digitText = String(digits)
        var result: String
        if k <= n && n <= 21 {
            result = digitText + String(repeating: "0", count: n - k)
        } else if 0 < n && n <= 21 {
            result = String(digits[0..<n]) + "." + String(digits[n...])
        } else if -6 < n && n <= 0 {
            result = "0." + String(repeating: "0", count: -n) + digitText
        } else {
            let e = n - 1
            let sign = e < 0 ? "-" : "+"
            let mantissa = k == 1 ? digitText : String(digits[0]) + "." + String(digits[1...])
            result = mantissa + "e" + sign + String(abs(e))
        }
        return negative ? "-" + result : result
    }

    // MARK: - Reading

    private struct Parser {
        let bytes: [UInt8]
        let lenient: Bool
        var index = 0

        var atEnd: Bool { index >= bytes.count }

        mutating func skipWhitespace() {
            while index < bytes.count,
                  bytes[index] == 0x20 || bytes[index] == 0x09 || bytes[index] == 0x0A || bytes[index] == 0x0D {
                index += 1
            }
        }

        mutating func value(depth: Int) throws -> BridgeJSON {
            guard depth <= BridgeCanonicalJSON.maxDepth else { throw BridgeCanonicalJSONError.tooDeep }
            guard index < bytes.count else { throw BridgeCanonicalJSONError.malformed("unexpected end") }
            switch bytes[index] {
            case UInt8(ascii: "{"): return try object(depth: depth)
            case UInt8(ascii: "["): return try array(depth: depth)
            case UInt8(ascii: "\""): return .string(try string())
            case UInt8(ascii: "t"): try literal("true"); return .bool(true)
            case UInt8(ascii: "f"): try literal("false"); return .bool(false)
            case UInt8(ascii: "n"): try literal("null"); return .null
            default:
                do {
                    return .number(try number())
                } catch BridgeCanonicalJSONError.invalidNumber where lenient {
                    return .null
                }
            }
        }

        mutating func literal(_ word: String) throws {
            let expected = Array(word.utf8)
            guard index + expected.count <= bytes.count,
                  Array(bytes[index..<index + expected.count]) == expected else {
                throw BridgeCanonicalJSONError.malformed("unexpected token")
            }
            index += expected.count
        }

        mutating func object(depth: Int) throws -> BridgeJSON {
            index += 1
            var members: [BridgeJSONMember] = []
            var seen = Set<String>()
            skipWhitespace()
            if index < bytes.count, bytes[index] == UInt8(ascii: "}") {
                index += 1
                return .object(members)
            }
            while true {
                skipWhitespace()
                guard index < bytes.count, bytes[index] == UInt8(ascii: "\"") else {
                    throw BridgeCanonicalJSONError.malformed("an object member needs a name")
                }
                let key = try string()
                // The names are compared after the escapes are undone:
                // `"a"` and `"a"` are the same name.
                guard seen.insert(key).inserted || lenient else {
                    throw BridgeCanonicalJSONError.duplicateKey(key)
                }
                skipWhitespace()
                guard index < bytes.count, bytes[index] == UInt8(ascii: ":") else {
                    throw BridgeCanonicalJSONError.malformed("an object member needs a colon")
                }
                index += 1
                skipWhitespace()
                let member = try value(depth: depth + 1)
                members.append(BridgeJSONMember(key: key, value: member))
                skipWhitespace()
                guard index < bytes.count else { throw BridgeCanonicalJSONError.malformed("unexpected end") }
                if bytes[index] == UInt8(ascii: ",") {
                    index += 1
                    continue
                }
                if bytes[index] == UInt8(ascii: "}") {
                    index += 1
                    return .object(members)
                }
                throw BridgeCanonicalJSONError.malformed("unexpected token in an object")
            }
        }

        mutating func array(depth: Int) throws -> BridgeJSON {
            index += 1
            var items: [BridgeJSON] = []
            skipWhitespace()
            if index < bytes.count, bytes[index] == UInt8(ascii: "]") {
                index += 1
                return .array(items)
            }
            while true {
                skipWhitespace()
                items.append(try value(depth: depth + 1))
                skipWhitespace()
                guard index < bytes.count else { throw BridgeCanonicalJSONError.malformed("unexpected end") }
                if bytes[index] == UInt8(ascii: ",") {
                    index += 1
                    continue
                }
                if bytes[index] == UInt8(ascii: "]") {
                    index += 1
                    return .array(items)
                }
                throw BridgeCanonicalJSONError.malformed("unexpected token in an array")
            }
        }

        mutating func string() throws -> String {
            index += 1
            var scalars = String.UnicodeScalarView()
            var raw: [UInt8] = []
            func flushRaw() throws {
                guard !raw.isEmpty else { return }
                // Decoding replaces an invalid sequence, so a text whose bytes
                // changed was not valid UTF-8.
                let text = String(decoding: raw, as: UTF8.self)
                guard text.utf8.elementsEqual(raw) else {
                    throw BridgeCanonicalJSONError.malformed("a string is not valid UTF-8")
                }
                scalars.append(contentsOf: text.unicodeScalars)
                raw.removeAll(keepingCapacity: true)
            }
            while true {
                guard index < bytes.count else { throw BridgeCanonicalJSONError.malformed("unterminated string") }
                let byte = bytes[index]
                if byte == UInt8(ascii: "\"") {
                    index += 1
                    try flushRaw()
                    return String(scalars)
                }
                if byte < 0x20 {
                    throw BridgeCanonicalJSONError.malformed("a string carries a raw control character")
                }
                if byte != UInt8(ascii: "\\") {
                    raw.append(byte)
                    index += 1
                    continue
                }
                try flushRaw()
                index += 1
                guard index < bytes.count else { throw BridgeCanonicalJSONError.malformed("unterminated escape") }
                let escape = bytes[index]
                index += 1
                switch escape {
                case UInt8(ascii: "\""): scalars.append("\"")
                case UInt8(ascii: "\\"): scalars.append("\\")
                case UInt8(ascii: "/"): scalars.append("/")
                case UInt8(ascii: "b"): scalars.append("\u{08}")
                case UInt8(ascii: "f"): scalars.append("\u{0C}")
                case UInt8(ascii: "n"): scalars.append("\u{0A}")
                case UInt8(ascii: "r"): scalars.append("\u{0D}")
                case UInt8(ascii: "t"): scalars.append("\u{09}")
                case UInt8(ascii: "u"):
                    let unit = try hex4()
                    if (0xD800...0xDBFF).contains(unit) {
                        // A high surrogate needs its low half right after it.
                        guard index + 1 < bytes.count, bytes[index] == UInt8(ascii: "\\"),
                              bytes[index + 1] == UInt8(ascii: "u") else {
                            throw BridgeCanonicalJSONError.malformed("a lone surrogate")
                        }
                        index += 2
                        let low = try hex4()
                        guard (0xDC00...0xDFFF).contains(low) else {
                            throw BridgeCanonicalJSONError.malformed("a lone surrogate")
                        }
                        let value = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                        scalars.append(Unicode.Scalar(value)!)
                    } else if (0xDC00...0xDFFF).contains(unit) {
                        throw BridgeCanonicalJSONError.malformed("a lone surrogate")
                    } else {
                        scalars.append(Unicode.Scalar(unit)!)
                    }
                default:
                    throw BridgeCanonicalJSONError.malformed("an unknown escape")
                }
            }
        }

        mutating func hex4() throws -> UInt32 {
            guard index + 4 <= bytes.count else { throw BridgeCanonicalJSONError.malformed("a short escape") }
            var value: UInt32 = 0
            for byte in bytes[index..<index + 4] {
                let digit: UInt32
                switch byte {
                case UInt8(ascii: "0")...UInt8(ascii: "9"): digit = UInt32(byte - UInt8(ascii: "0"))
                case UInt8(ascii: "a")...UInt8(ascii: "f"): digit = UInt32(byte - UInt8(ascii: "a") + 10)
                case UInt8(ascii: "A")...UInt8(ascii: "F"): digit = UInt32(byte - UInt8(ascii: "A") + 10)
                default: throw BridgeCanonicalJSONError.malformed("a bad escape")
                }
                value = value * 16 + digit
            }
            index += 4
            return value
        }

        mutating func number() throws -> Double {
            let start = index
            func digits() -> Int {
                let first = index
                while index < bytes.count, (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(bytes[index]) {
                    index += 1
                }
                return index - first
            }
            if index < bytes.count, bytes[index] == UInt8(ascii: "-") { index += 1 }
            guard index < bytes.count else { throw BridgeCanonicalJSONError.malformed("unexpected end") }
            if bytes[index] == UInt8(ascii: "0") {
                index += 1
            } else if digits() == 0 {
                throw BridgeCanonicalJSONError.malformed("unexpected token")
            }
            var significantNonZero = bytes[start..<index].contains { (UInt8(ascii: "1")...UInt8(ascii: "9")).contains($0) }
            if index < bytes.count, bytes[index] == UInt8(ascii: ".") {
                index += 1
                let fractionStart = index
                guard digits() > 0 else { throw BridgeCanonicalJSONError.malformed("a fraction needs digits") }
                if bytes[fractionStart..<index].contains(where: { $0 != UInt8(ascii: "0") }) {
                    significantNonZero = true
                }
            }
            if index < bytes.count, bytes[index] == UInt8(ascii: "e") || bytes[index] == UInt8(ascii: "E") {
                index += 1
                if index < bytes.count, bytes[index] == UInt8(ascii: "+") || bytes[index] == UInt8(ascii: "-") {
                    index += 1
                }
                guard digits() > 0 else { throw BridgeCanonicalJSONError.malformed("an exponent needs digits") }
            }
            let literal = String(decoding: bytes[start..<index], as: UTF8.self)
            guard let value = Double(literal), value.isFinite else {
                throw BridgeCanonicalJSONError.invalidNumber(literal)
            }
            // A non-zero literal that reads as zero lost all of its value:
            // it is not a number a binary64 holds.
            if value == 0 && significantNonZero {
                throw BridgeCanonicalJSONError.invalidNumber(literal)
            }
            return value == 0 ? 0 : value
        }
    }
}
