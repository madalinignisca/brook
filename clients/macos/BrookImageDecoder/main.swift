import Foundation
import Security

// The broker (spec 2026-09-26-mac-previews-spec.md §1): long-lived, sandboxed with no
// entitlements beyond the sandbox key, and it never decodes. Each image goes to a fresh
// worker process; the broker only moves bytes and checks the worker's fixed-format reply.

/// The broker's own team (empty for an ad-hoc build), from its signature at runtime.
func ownTeam() -> String? {
    var code: SecCode?
    guard SecCodeCopySelf([], &code) == errSecSuccess, let code else { return nil }
    var staticCode: SecStaticCode?
    guard SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode else { return nil }
    var info: CFDictionary?
    guard SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &info)
        == errSecSuccess
    else { return nil }
    return (info as? [String: Any])?[kSecCodeInfoTeamIdentifier as String] as? String
}

/// Only Brook.app may connect: its identifier and an Apple-anchored certificate of this
/// build's own team. A worker can't act as a client.
///
/// Ad-hoc builds (Debug, no team) get no requirement: under test, the host app carries
/// injected code, so its dynamic signature doesn't validate and even an identifier-only
/// requirement refuses it (measured). A Release broker without a team refuses to run.
func peerRequirement() -> String? {
    if let team = ownTeam(), !team.isEmpty {
        return "identifier \"dev.brook.Brook\" and anchor apple generic"
            + " and certificate leaf[subject.OU] = \"\(team)\""
    }
    #if DEBUG
    return nil
    #else
    exit(1)
    #endif
}

final class Broker: NSObject, NSXPCListenerDelegate {
    func listener(_ listener: NSXPCListener, shouldAcceptNewConnection c: NSXPCConnection) -> Bool {
        let interface = NSXPCInterface(with: ImageDecoding.self)
        // Plain values only, both ways: Data and numbers.
        let data = NSSet(array: [NSData.self]) as! Set<AnyHashable>
        interface.setClasses(data, for: #selector(ImageDecoding.decode(_:kind:reply:)), argumentIndex: 0, ofReply: false)
        interface.setClasses(data, for: #selector(ImageDecoding.decode(_:kind:reply:)), argumentIndex: 3, ofReply: true)
        let session = Session()
        c.exportedInterface = interface
        c.exportedObject = session
        // The connection gone (the app's timeout, a row gone): its workers are killed.
        c.invalidationHandler = { session.cancelAll() }
        c.interruptionHandler = { session.cancelAll() }
        c.resume()
        return true
    }
}

/// One connection's requests: each runs on its own, and ends its worker when the connection
/// goes.
final class Session: NSObject, ImageDecoding, @unchecked Sendable {
    private let lock = NSLock()
    private var running: [UUID: WorkerRun] = [:]
    private static let queue = DispatchQueue(label: "dev.brook.decode", attributes: .concurrent)

    func decode(_ bytes: Data, kind: Int, reply: @escaping (Int, Int, Int, Data) -> Void) {
        // Returns at once: NSXPC delivers on the connection's serial queue, and a blocking
        // decode would start the next request's deadline late.
        let answer = ReplyOnce(reply)
        guard let code = ImageKindCode(request: kind), bytes.count <= PreviewCaps.maxInputBytes else {
            return answer.send(.refused)
        }
        let id = UUID()
        Self.queue.async { [self] in
            let run = WorkerRun(kind: code, bytes: bytes)
            lock.withLock { running[id] = run }
            let result = run.finish()
            lock.withLock { _ = running.removeValue(forKey: id) }
            answer.send(result)
        }
    }

    func cancelAll() {
        let runs = lock.withLock { Array(running.values) }
        for run in runs { run.kill() }
    }
}

/// The reply block, called exactly once.
final class ReplyOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var reply: ((Int, Int, Int, Data) -> Void)?
    init(_ reply: @escaping (Int, Int, Int, Data) -> Void) { self.reply = reply }

    func send(_ code: ReplyCode) { send(Frame(code: UInt32(code.rawValue), width: 0, height: 0, body: Data())) }

    func send(_ frame: Frame) {
        let r = lock.withLock { () -> ((Int, Int, Int, Data) -> Void)? in
            defer { reply = nil }
            return reply
        }
        r?(Int(frame.code), Int(frame.width), Int(frame.height), frame.body)
    }
}

// Start: an empty temp directory (a worker of an earlier run could have left files there),
// then listen.
if let items = try? FileManager.default.contentsOfDirectory(atPath: NSTemporaryDirectory()) {
    for item in items { try? FileManager.default.removeItem(atPath: NSTemporaryDirectory() + item) }
}
let requirement = peerRequirement()
let broker = Broker()
let listener = NSXPCListener.service()
if let requirement { listener.setConnectionCodeSigningRequirement(requirement) }
listener.delegate = broker
listener.resume()
