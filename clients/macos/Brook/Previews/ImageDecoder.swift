import CoreGraphics
import Foundation
import os

/// The app's side of the decoder (spec §3, §4): one XPC connection per request (a timeout
/// ends only its own worker), at most two in flight, the newest waiting request first, and
/// requests whose row has gone dropped before they start. Every reply is validated.
actor ImageDecoder {
    static let shared = ImageDecoder()

    /// One request to the decoder: the reply `(code, width, height, rgba)`, or nil when the
    /// service can't be reached. The real one is `request(bytes:kind:)`; tests pass a fake.
    typealias Request = @Sendable (Data, ImageKindCode) async -> (Int, Int, Int, Data)?
    private let send: Request

    init(send: @escaping Request = { await ImageDecoder.request(bytes: $0, kind: $1) }) {
        self.send = send
    }

    static let inFlight = 2
    static let timeout: Duration = .seconds(10)

    /// A request's row is still shown.
    typealias Alive = @Sendable () -> Bool

    private struct Waiting {
        let alive: Alive
        let start: CheckedContinuation<Bool, Never>
    }

    private var running = 0
    private var waiting: [Waiting] = []
    private var disabled = false
    /// Requests in a row that got no answer at all. One can be a broker killed under memory
    /// pressure (it relaunches); several mean the service can't be reached.
    private var unreachable = 0
    static let unreachableLimit = 3
    private let log = Logger(subsystem: "dev.brook.Brook", category: "previews")

    /// A thumbnail for `bytes` (`FfiImagePreview`'s), or nil: no preview, whatever the
    /// reason. Never throws; previews are optional.
    func thumbnail(bytes: Data, kind: ImageKindCode, header: (width: Int, height: Int),
                   alive: @escaping Alive) async -> CGImage? {
        guard !disabled, await slot(alive) else { return nil }
        defer { release() }
        guard alive() else { return nil }
        let reply = await send(bytes, kind)
        switch reply {
        case .none:
            unreachable += 1
            if unreachable >= Self.unreachableLimit { disableOnce("the decoder couldn't be reached") }
            return nil
        case let .some((code, w, h, rgba)):
            unreachable = 0
            return PreviewValidator.image(code: code, width: w, height: h, rgba: rgba, header: header)
        }
    }

    // ---- The queue ----

    /// Wait for a slot: true when it's this request's turn and its row is still there.
    private func slot(_ alive: @escaping Alive) async -> Bool {
        if running < Self.inFlight {
            running += 1
            return true
        }
        let go = await withCheckedContinuation { waiting.append(Waiting(alive: alive, start: $0)) }
        return go
    }

    private func release() {
        // The newest waiting request whose row is still shown; the gone are let go.
        while let next = waiting.popLast() {
            if next.alive() {
                next.start.resume(returning: true)
                return
            }
            next.start.resume(returning: false)
        }
        running -= 1
    }

    private func disableOnce(_ why: String) {
        guard !disabled else { return }
        disabled = true
        log.info("image previews are off: \(why, privacy: .public)")
    }

    // ---- One request ----

    /// One request on its own connection, answered once: the reply, a timeout (nil code
    /// `timeout`), or nil if the service couldn't be reached at all.
    static func request(bytes: Data, kind: ImageKindCode) async -> (Int, Int, Int, Data)? {
        let connection = NSXPCConnection(serviceName: imageDecoderServiceName)
        connection.remoteObjectInterface = Self.interface()
        connection.resume()
        defer { connection.invalidate() } // ends the worker if it's still running
        return await withCheckedContinuation { (done: CheckedContinuation<(Int, Int, Int, Data)?, Never>) in
            let once = OnceBox(done)
            let timer = Task {
                try? await Task.sleep(for: timeout)
                if !Task.isCancelled { once.resume((ReplyCode.timeout.rawValue, 0, 0, Data())) }
            }
            once.onResume = { timer.cancel() } // no sleeping task left behind
            let proxy = connection.remoteObjectProxyWithErrorHandler { _ in once.resume(nil) }
            guard let decoder = proxy as? ImageDecoding else { return once.resume(nil) }
            decoder.decode(bytes, kind: Int(kind.rawValue)) { code, w, h, rgba in once.resume((code, w, h, rgba)) }
        }
    }

    /// The protocol with its classes pinned to plain data.
    static func interface() -> NSXPCInterface {
        let interface = NSXPCInterface(with: ImageDecoding.self)
        let data = NSSet(array: [NSData.self]) as! Set<AnyHashable>
        interface.setClasses(data, for: #selector(ImageDecoding.decode(_:kind:reply:)), argumentIndex: 0, ofReply: false)
        interface.setClasses(data, for: #selector(ImageDecoding.decode(_:kind:reply:)), argumentIndex: 3, ofReply: true)
        return interface
    }
}

/// A continuation resumed once, whoever comes first (the reply, an error, the timeout).
private final class OnceBox<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<T, Never>?
    /// Runs once, on the first resume.
    var onResume: (@Sendable () -> Void)? {
        get { lock.withLock { hook } }
        set { lock.withLock { hook = newValue } }
    }
    private var hook: (@Sendable () -> Void)?
    init(_ c: CheckedContinuation<T, Never>) { continuation = c }
    func resume(_ value: T) {
        let (c, h) = lock.withLock { () -> (CheckedContinuation<T, Never>?, (@Sendable () -> Void)?) in
            defer { continuation = nil; hook = nil }
            return (continuation, continuation == nil ? nil : hook)
        }
        c?.resume(returning: value)
        h?()
    }
}
