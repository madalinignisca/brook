import Darwin
import Foundation

/// One worker process for one image: spawned with only fds 0 to 2, fed on its own thread,
/// read against a deadline, killed by pid at the deadline or on a bad frame, and always
/// reaped by pid.
final class WorkerRun: @unchecked Sendable {
    static let deadline: TimeInterval = 8

    private let kind: ImageKindCode
    private let bytes: Data
    private let lock = NSLock()
    private var pid: pid_t = 0
    private var killed = false

    init(kind: ImageKindCode, bytes: Data) {
        self.kind = kind
        self.bytes = bytes
    }

    /// Kill the worker (the connection went, or the deadline passed): by pid first, since
    /// `setpgid` could have moved it out of its group without a fork, then its group.
    func kill() {
        lock.withLock {
            guard pid > 0, !killed else { return }
            killed = true
            Darwin.kill(pid, SIGKILL)
            killpg(pid, SIGKILL)
        }
    }

    func finish() -> Frame {
        func failed(_ code: ReplyCode) -> Frame { Frame(code: UInt32(code.rawValue), width: 0, height: 0, body: Data()) }
        guard let path = Bundle.main.path(forAuxiliaryExecutable: "BrookImageWorker") else { return failed(.workerFailed) }
        var toWorker: [Int32] = [0, 0], fromWorker: [Int32] = [0, 0]
        guard pipe(&toWorker) == 0 else { return failed(.workerFailed) }
        guard pipe(&fromWorker) == 0 else {
            close(toWorker[0]); close(toWorker[1])
            return failed(.workerFailed)
        }

        // Only 0, 1 and 2 reach the worker (CLOEXEC_DEFAULT closes everything else, so it
        // never sees another image's pipes); it leads its own process group.
        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        posix_spawn_file_actions_adddup2(&actions, toWorker[0], 0)
        posix_spawn_file_actions_adddup2(&actions, fromWorker[1], 1)
        posix_spawn_file_actions_addopen(&actions, 2, "/dev/null", O_WRONLY, 0)
        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETPGROUP))
        posix_spawnattr_setpgroup(&attr, 0)
        let argv: [UnsafeMutablePointer<CChar>?] = [strdup(path), nil]
        var child: pid_t = 0
        let spawned = posix_spawn(&child, path, &actions, &attr, argv, environ)
        posix_spawn_file_actions_destroy(&actions)
        posix_spawnattr_destroy(&attr)
        free(argv[0])
        // The child's ends, closed here at once: a kept write end would mean EOF never comes.
        close(toWorker[0])
        close(fromWorker[1])
        guard spawned == 0 else {
            close(toWorker[1]); close(fromWorker[0])
            return failed(.workerFailed)
        }
        lock.withLock { pid = child }
        let deadline = DispatchTime.now() + Self.deadline // from the spawn, so writing counts

        // The input, on its own thread; a worker that exits early costs a write error, never
        // a SIGPIPE here.
        let writer = toWorker[1]
        _ = fcntl(writer, F_SETNOSIGPIPE, 1)
        let request = Data([kind.rawValue]) + bytes
        let wrote = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            request.withUnsafeBytes { buf in
                var off = 0
                while off < buf.count {
                    let n = write(writer, buf.baseAddress! + off, buf.count - off)
                    if n < 0 && errno == EINTR { continue }
                    if n <= 0 { break }
                    off += n
                }
            }
            close(writer)
            wrote.signal()
        }

        let read = Frame.read(fd: fromWorker[0], deadline: deadline)
        let frame: Frame
        switch read {
        case .success(let f): frame = f
        case .failure(.timeout):
            kill()
            frame = failed(.timeout)
        case .failure(.malformed):
            kill()
            frame = failed(.badFrame)
        }
        close(fromWorker[0])
        wrote.wait() // the writer ends once the worker is gone (its read end closed)

        // Always reaped, by pid.
        var status: Int32 = 0
        var usage = rusage()
        while wait4(child, &status, 0, &usage) < 0 && errno == EINTR {}
        let wasKilled = lock.withLock { killed }
        if !wasKilled {
            // Exited on its own: only a clean exit counts.
            let exitedCleanly = (status & 0x7f) == 0 && ((status >> 8) & 0xff) == 0
            if !exitedCleanly { return failed(.workerFailed) }
        }
        #if DEBUG
        if frame.code == UInt32(ReplyCode.timeout.rawValue) {
            // For the tests: the reap, and whether anything is left in the worker's group.
            let group = killpg(child, 0) == 0 ? "alive" : (errno == ESRCH ? "empty" : "errno\(errno)")
            let report = "reaped=\(child)\ngroup=\(group)\nmaxrss=\(usage.ru_maxrss)"
            return Frame(code: frame.code, width: 0, height: 0, body: Data(report.utf8))
        }
        if frame.code == UInt32(ReplyCode.report.rawValue) {
            let extra = "\nmaxrss=\(usage.ru_maxrss)\nbrokerPid=\(getpid())"
            return Frame(code: frame.code, width: 0, height: 0, body: frame.body + Data(extra.utf8))
        }
        #endif
        return frame
    }
}
