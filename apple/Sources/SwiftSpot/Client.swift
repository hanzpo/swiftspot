import CSwiftSpot
import Foundation

/// SwiftSpot's engine, linked into this app (src/ffi.rs): requests go in
/// as JSON and await their reply; events arrive on `onEvent`. The engine
/// runs on threads of its own and there is one per process.
final class Client: @unchecked Sendable {
    enum Failure: Error, LocalizedError {
        case notRunning
        case server(String)
        case closed

        var errorDescription: String? {
            switch self {
            case .notRunning: "SwiftSpot's engine didn't start."
            case .server(let message): message
            case .closed: "SwiftSpot's engine stopped."
            }
        }
    }

    /// The process's engine, started on first use.
    @MainActor static let shared = Client()

    private var engine: OpaquePointer?
    private let lock = NSLock()
    private var nextID = 1
    private var waiting: [Int: CheckedContinuation<Data, Error>] = [:]
    /// Called on the main actor with each event's name and its data as JSON.
    var onEvent: (@MainActor @Sendable (String, Data) -> Void)?

    private init() {
        let context = Unmanaged.passRetained(self).toOpaque()
        engine = swiftspot_start({ context, data, count in
            guard let context else { return }
            let client = Unmanaged<Client>.fromOpaque(context).takeUnretainedValue()
            if let data {
                client.handle(line: Data(bytes: data, count: count))
            } else {
                client.closed()
            }
        }, context)
    }

    var running: Bool {
        lock.lock()
        defer { lock.unlock() }
        return engine != nil
    }

    /// Saves the state and stops the engine, for quitting.
    func stop() {
        lock.lock()
        let engine = self.engine
        self.engine = nil
        lock.unlock()
        if let engine { swiftspot_stop(engine) }
    }

    /// Calls `method` and returns its `result` as JSON.
    @MainActor
    func call(_ method: String, _ params: [String: Any] = [:]) async throws -> Data {
        let id = takeID()
        let line = try encode(id: id, method: method, params: params)
        return try await withCheckedThrowingContinuation { continuation in
            lock.lock()
            waiting[id] = continuation
            lock.unlock()
            if !write(line) {
                finish(id, .failure(Failure.closed))
            }
        }
    }

    /// Calls `method` and decodes its result off the main actor, so a long
    /// list never holds up the interface while it decodes.
    @MainActor
    func call<T: Decodable & Sendable>(_ method: String, _ params: [String: Any] = [:], as type: T.Type) async throws -> T {
        try await Self.decode(T.self, from: try await call(method, params))
    }

    @concurrent
    private static func decode<T: Decodable & Sendable>(_ type: T.Type, from data: Data) async throws -> T {
        try JSONDecoder.api.decode(T.self, from: data)
    }

    /// Fire and forget, for the player's buttons: the reply is dropped.
    @MainActor
    func send(_ method: String, _ params: [String: Any] = [:]) {
        if let line = try? encode(id: takeID(), method: method, params: params) {
            _ = write(line)
        }
    }

    private func takeID() -> Int {
        lock.lock()
        defer { lock.unlock() }
        nextID += 1
        return nextID
    }

    private func encode(id: Int, method: String, params: [String: Any]) throws -> Data {
        try JSONSerialization.data(
            withJSONObject: ["id": id, "method": method, "params": params] as [String: Any])
    }

    private func write(_ data: Data) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let engine else { return false }
        data.withUnsafeBytes { raw in
            swiftspot_send(engine, raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
        }
        return true
    }

    private func finish(_ id: Int, _ result: Result<Data, Error>) {
        lock.lock()
        let continuation = waiting.removeValue(forKey: id)
        lock.unlock()
        continuation?.resume(with: result)
    }

    private func closed() {
        lock.lock()
        let pending = waiting
        waiting.removeAll()
        lock.unlock()
        for continuation in pending.values { continuation.resume(throwing: Failure.closed) }
    }

    /// Dispatches a JSON line in a single pass by slicing top-level values
    /// directly out of the UTF-8 buffer, avoiding `JSONSerialization`
    /// Foundation-object round-trips before `JSONDecoder` runs.
    private func handle(line: Data) {
        let fields = Self.topLevelFields(in: line)
        if let eventRange = fields["event"],
           let event = try? JSONDecoder.api.decode(String.self, from: line[eventRange]) {
            let payload = fields["data"].map { Data(line[$0]) } ?? Data("null".utf8)
            let onEvent = onEvent
            Task { @MainActor in onEvent?(event, payload) }
            return
        }
        guard let idRange = fields["id"],
              let id = try? JSONDecoder.api.decode(Int.self, from: line[idRange])
        else { return }
        if let errorRange = fields["error"],
           let error = try? JSONDecoder.api.decode(String.self, from: line[errorRange]) {
            finish(id, .failure(Failure.server(error)))
        } else if let resultRange = fields["result"] {
            finish(id, .success(Data(line[resultRange])))
        } else {
            finish(id, .success(Data("null".utf8)))
        }
    }

    /// Returns the raw byte range of each top-level member value in a JSON
    /// object without materialising nested objects or arrays.
    static func topLevelFields(in data: Data) -> [String: Range<Int>] {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return [:] }
            let count = raw.count
            var i = 0

            func skipWhitespace() {
                while i < count {
                    switch base[i] {
                    case 0x20, 0x09, 0x0A, 0x0D: i += 1
                    default: return
                    }
                }
            }

            func skipString() -> Bool {
                guard i < count, base[i] == 0x22 else { return false }
                i += 1
                while i < count {
                    let b = base[i]
                    i += 1
                    if b == 0x5C { // backslash
                        if i < count { i += 1 }
                    } else if b == 0x22 { // closing quote
                        return true
                    }
                }
                return false
            }

            func skipValue() -> Range<Int>? {
                skipWhitespace()
                guard i < count else { return nil }
                let start = i
                switch base[i] {
                case 0x22: // string
                    guard skipString() else { return nil }
                    return start..<i
                case 0x7B, 0x5B: // '{' or '['
                    var depth = 0
                    while i < count {
                        switch base[i] {
                        case 0x22:
                            guard skipString() else { return nil }
                        case 0x7B, 0x5B:
                            depth += 1
                            i += 1
                        case 0x7D, 0x5D:
                            depth -= 1
                            i += 1
                            if depth == 0 { return start..<i }
                        default:
                            i += 1
                        }
                    }
                    return nil
                default: // number, bool, null
                    while i < count {
                        switch base[i] {
                        case 0x2C, 0x7D, 0x5D, 0x20, 0x09, 0x0A, 0x0D:
                            return start..<i
                        default:
                            i += 1
                        }
                    }
                    return start..<i
                }
            }

            skipWhitespace()
            guard i < count, base[i] == 0x7B else { return [:] } // '{'
            i += 1
            var result: [String: Range<Int>] = [:]
            while i < count {
                skipWhitespace()
                if i < count, base[i] == 0x7D { break } // '}'
                let keyStart = i
                guard skipString() else { break }
                // Top-level protocol keys are plain ASCII without escapes.
                let key = String(decoding: UnsafeBufferPointer(start: base + keyStart + 1, count: i - keyStart - 2), as: UTF8.self)
                skipWhitespace()
                guard i < count, base[i] == 0x3A else { break } // ':'
                i += 1
                guard let valueRange = skipValue() else { break }
                result[key] = valueRange
                skipWhitespace()
                if i < count, base[i] == 0x2C { // ','
                    i += 1
                } else {
                    break
                }
            }
            return result
        }
    }
}

extension JSONDecoder {
    static let api: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }()
}
