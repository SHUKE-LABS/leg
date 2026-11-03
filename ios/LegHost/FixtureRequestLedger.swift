import Foundation

/// The durable record of the requests the fixture accepted.
///
/// The ledger is a file of one JSON object per line. Every object holds the
/// turn the request belongs to, the ordinal of the request in that turn and
/// the status it was answered with. No body, no header and no credential
/// ever reaches the file.
///
/// The ledger is cumulative: it is read when it is opened, so a second run
/// of the host sees the requests of the first one. A run that starts again
/// therefore cannot hide a duplicate behind a fresh counter.
final class FixtureRequestLedger {
    /// One accepted request.
    struct Entry: Equatable {
        /// The turn the request belongs to.
        let turn: String
        /// The ordinal of the request in that turn.
        let request: Int
        /// The status the request was answered with.
        let status: Int
    }

    /// The file the ledger is written to.
    let url: URL

    private let lock = NSLock()
    private var entries: [Entry] = []

    /// Opens the ledger and reads the entries it holds.
    init(url: URL) {
        self.url = url
        entries = FixtureRequestLedger.read(url)
    }

    /// The number of requests the ledger holds.
    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return entries.count
    }

    /// The number of turns the ledger holds.
    ///
    /// A run that starts again and accepts no request leaves the number
    /// where it was: the ledger holds the turn of the first run only.
    var turns: Int {
        lock.lock()
        defer { lock.unlock() }
        return Set(entries.map(\.turn)).count
    }

    /// The entries the ledger holds.
    var recordedEntries: [Entry] {
        lock.lock()
        defer { lock.unlock() }
        return entries
    }

    /// Removes the ledger.
    ///
    /// The ledger is cleared on the reset launch only, so the requests of
    /// the runs that follow are counted on top of the ones that are there.
    func clear() throws {
        lock.lock()
        defer { lock.unlock() }
        entries = []
        if FileManager.default.fileExists(atPath: url.path) {
            try FileManager.default.removeItem(at: url)
        }
    }

    /// Appends one accepted request to the ledger.
    func append(turn: String, request: Int, status: Int) throws {
        let entry = Entry(turn: turn, request: request, status: status)
        let line = try FixtureRequestLedger.encode(entry)
        lock.lock()
        defer { lock.unlock() }
        if !FileManager.default.fileExists(atPath: url.path) {
            FileManager.default.createFile(atPath: url.path, contents: nil)
        }
        let handle = try FileHandle(forWritingTo: url)
        defer { try? handle.close() }
        try handle.seekToEndOfFile()
        // The whole line is written in one call, so a reader never sees half
        // a record.
        try handle.write(contentsOf: line)
        entries.append(entry)
    }

    /// Writes one entry as a line of the ledger.
    private static func encode(_ entry: Entry) throws -> Data {
        let object: [String: Any] = [
            "turn": entry.turn,
            "request": entry.request,
            "status": entry.status,
        ]
        var line = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        line.append(0x0A)
        return line
    }

    /// Reads the entries the ledger file holds.
    ///
    /// A line that cannot be read is skipped: the ledger is evidence, and a
    /// broken line must not hide the records around it.
    private static func read(_ url: URL) -> [Entry] {
        guard let data = try? Data(contentsOf: url) else { return [] }
        var entries: [Entry] = []
        for line in data.split(separator: 0x0A) {
            guard let object = try? JSONSerialization.jsonObject(with: Data(line)) as? [String: Any],
                  let turn = object["turn"] as? String,
                  let request = object["request"] as? Int,
                  let status = object["status"] as? Int
            else {
                continue
            }
            entries.append(Entry(turn: turn, request: request, status: status))
        }
        return entries
    }
}
