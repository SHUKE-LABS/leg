import XCTest
@testable import LegHost

/// The RFC 8785 digest of a bridge input.
///
/// The vectors are the ones the Rust bridge checks, so the guest and the
/// host agree on every `input_sha256`.
final class BridgeCanonicalJSONTests: XCTestCase {
    private func canonical(_ wire: String) throws -> String {
        String(decoding: try BridgeCanonicalJSON.canonicalize(Data(wire.utf8)), as: UTF8.self)
    }

    private func digest(_ wire: String) throws -> String {
        BridgeCanonicalJSON.digest(try BridgeCanonicalJSON.parse(Data(wire.utf8)))
    }

    func testReorderedKeysHaveTheSharedDigest() throws {
        let wire = #"{"sort":"duration_desc","filter":{"media_type":"video"},"limit":20}"#
        XCTAssertEqual(try canonical(wire), #"{"filter":{"media_type":"video"},"limit":20,"sort":"duration_desc"}"#)
        XCTAssertEqual(try digest(wire), "a9a83ee45388e790690d5f5683782ecf738d5f703189c18b139a01f29c8bce4a")
    }

    func testDestinationVector() throws {
        XCTAssertEqual(try digest(#"{"destination":"guest_workspace"}"#),
                       "8045fd38b79820c6825bd8096a324dea1431cfe9e3e17870b1592de88295a750")
    }

    func testKeysSortByUTF16CodeUnits() throws {
        let wire = #"{"\uE000":"bmp","\uD834\uDD1E":"supp"}"#
        XCTAssertEqual(try canonical(wire), "{\"\u{1D11E}\":\"supp\",\"\u{E000}\":\"bmp\"}")
        XCTAssertEqual(try digest(wire), "73e96d65d5acff2a00259e91342ce76055ab13ce7809e00e9920246333341f59")
    }

    func testNumbersUseTheECMAScriptForm() throws {
        let wire = #"{"n":1.0,"small":1e-6,"tiny":1e-7,"huge":1e21,"negzero":-0}"#
        XCTAssertEqual(try canonical(wire), #"{"huge":1e+21,"n":1,"negzero":0,"small":0.000001,"tiny":1e-7}"#)
        XCTAssertEqual(try digest(wire), "c25482e66991d36fc4ac2b9d6aa6ede849b011530dbed02a79fda81c5974dd24")
    }

    func testNumberFormatting() {
        XCTAssertEqual(BridgeCanonicalJSON.formatNumber(0.1), "0.1")
        XCTAssertEqual(BridgeCanonicalJSON.formatNumber(123456789012345680000), "123456789012345680000")
        XCTAssertEqual(BridgeCanonicalJSON.formatNumber(1.5e-7), "1.5e-7")
        XCTAssertEqual(BridgeCanonicalJSON.formatNumber(-2.5), "-2.5")
    }

    func testDuplicateNamesFailBeforeMaterialization() {
        for wire in [#"{"a":1,"a":2}"#, #"{"a":1,"\u0061":2}"#, #"{"x":{"b":1,"b":1}}"#] {
            XCTAssertThrowsError(try BridgeCanonicalJSON.parse(Data(wire.utf8)), wire) { error in
                guard case BridgeCanonicalJSONError.duplicateKey = error else {
                    return XCTFail("\(wire): \(error)")
                }
            }
        }
    }

    func testALenientReadOnlyRecoversTheEnvelope() throws {
        let value = try BridgeCanonicalJSON.parse(Data(#"{"request_id":"r","input":{"a":1,"a":2,"n":1e400}}"#.utf8),
                                                  lenient: true)
        XCTAssertEqual(value["request_id"]?.stringValue, "r")
        XCTAssertEqual(value["input"]?["n"], .null)
        XCTAssertThrowsError(try BridgeCanonicalJSON.parse(Data("not json".utf8), lenient: true))
    }

    func testNonIJSONNumbersAreRefused() {
        for wire in [#"{"n":1e400}"#, #"{"n":-1e400}"#, #"{"n":01}"#, #"{"n":1.}"#, #"{"n":NaN}"#] {
            XCTAssertThrowsError(try BridgeCanonicalJSON.parse(Data(wire.utf8)), wire)
        }
    }

    func testMalformedTextIsRefused() {
        let cases: [Data] = [
            Data(#"{"a":"\uD800"}"#.utf8),
            Data(#"{"a":1}x"#.utf8),
            Data([0x7B, 0x22, 0x61, 0x22, 0x3A, 0x22, 0xFF, 0x22, 0x7D]),
            Data(("{\"a\":" + String(repeating: "[", count: 80) + String(repeating: "]", count: 80) + "}").utf8),
        ]
        for data in cases {
            XCTAssertThrowsError(try BridgeCanonicalJSON.parse(data))
        }
    }

    func testStringEscaping() throws {
        XCTAssertEqual(try canonical(#"{"s":"\u0001\n\"\\\/é"}"#), "{\"s\":\"\\u0001\\n\\\"\\\\/é\"}")
    }

    func testChangedValueChangesTheDigest() throws {
        XCTAssertNotEqual(try digest(#"{"limit":20}"#), try digest(#"{"limit":21}"#))
        XCTAssertEqual(try digest(#"{"a":1,"b":2}"#), try digest(#"{ "b" : 2 , "a" : 1 }"#))
    }
}
