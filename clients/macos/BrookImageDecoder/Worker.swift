import Darwin
import Foundation

/// One worker process for one image: spawned with only fds 0 to 2 and an empty environment,
/// fed on its own thread, read against a deadline, and gone by the deadline whatever it does:
/// killed by pid if it hasn't exited, and always reaped by pid.
final class WorkerRun: @unchecked Sendable {
    static let deadline: TimeInterval = 8
    /// At most this many workers at once in this broker, whatever its clients ask.
    static let slots = DispatchSemaphore(value: 2)

    private let kind: ImageKindCode
    private let bytes: Data
    private let lock = NSLock()
    /// The live worker's pid, 0 before the spawn and after it's known to have exited (so a
    /// kill never reaches a pid the system has handed to another process).
    private var pid: pid_t = 0
    private var cancelled = false
    private var killed = false

    init(kind: ImageKindCode, bytes: Data) {
        self.kind = kind
        self.bytes = bytes
    }

    /// Stop this run: its connection went, or its deadline passed. A worker not spawned yet
    /// is killed the moment it is.
    func cancel() {
        lock.withLock {
            cancelled = true
            killLocked()
        }
    }

    /// By pid first, since `setpgid` could move the worker out of its group without a fork,
    /// then its group. Only while it's known to be running.
    private func killLocked() {
        guard pid > 0, !killed else { return }
        killed = true
        Darwin.kill(pid, SIGKILL)
        killpg(pid, SIGKILL)
    }

    func finish() -> Frame {
        func failed(_ code: ReplyCode) -> Frame { Frame(code: UInt32(code.rawValue), width: 0, height: 0, body: Data()) }
        if lock.withLock({ cancelled }) { return failed(.workerFailed) }
        guard let path = Bundle.main.path(forAuxiliaryExecutable: "BrookImageWorker") else { return failed(.workerFailed) }
        var toWorker: [Int32] = [0, 0], fromWorker: [Int32] = [0, 0]
        guard pipe(&toWorker) == 0 else { return failed(.workerFailed) }
        guard pipe(&fromWorker) == 0 else {
            close(toWorker[0]); close(toWorker[1])
            return failed(.workerFailed)
        }

        // Only 0, 1 and 2 reach the worker (CLOEXEC_DEFAULT closes everything else, so it
        // never sees another image's pipes); it leads its own process group; its environment
        // is empty.
        var actions: posix_spawn_file_actions_t?
        var attr: posix_spawnattr_t?
        var ready = posix_spawn_file_actions_init(&actions) == 0
        defer { posix_spawn_file_actions_destroy(&actions) }
        ready = ready && posix_spawnattr_init(&attr) == 0
        defer { posix_spawnattr_destroy(&attr) }
        ready = ready
            && posix_spawn_file_actions_adddup2(&actions, toWorker[0], 0) == 0
            && posix_spawn_file_actions_adddup2(&actions, fromWorker[1], 1) == 0
            && posix_spawn_file_actions_addopen(&actions, 2, "/dev/null", O_WRONLY, 0) == 0
            && posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETPGROUP)) == 0
            && posix_spawnattr_setpgroup(&attr, 0) == 0
        let argv: [UnsafeMutablePointer<CChar>?] = [strdup(path), nil]
        let envp: [UnsafeMutablePointer<CChar>?] = [nil]
        var child: pid_t = 0
        let spawned = ready ? posix_spawn(&child, path, &actions, &attr, argv, envp) : -1
        free(argv[0])
        // The child's ends, closed here at once: a kept write end would mean EOF never comes.
        close(toWorker[0])
        close(fromWorker[1])
        guard spawned == 0 else {
            close(toWorker[1]); close(fromWorker[0])
            return failed(.workerFailed)
        }
        lock.withLock {
            pid = child
            if cancelled { killLocked() } // cancelled while it was being spawned
        }
        let deadline = DispatchTime.now() + Self.deadline // from the spawn, so writing counts

        // The input, on its own thread; a worker that exits early (or is killed) costs a
        // write error, never a SIGPIPE here.
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

        var frame: Frame
        switch Frame.read(fd: fromWorker[0], deadline: deadline) {
        case .success(let f): frame = f
        case .failure(.timeout): frame = failed(.timeout)
        case .failure(.malformed): frame = failed(.badFrame)
        }
        close(fromWorker[0])
        if frame.code == UInt32(ReplyCode.timeout.rawValue) || frame.code == UInt32(ReplyCode.badFrame.rawValue) {
            lock.withLock { killLocked() }
        }

        // It must be gone by the deadline even after a good frame (a worker could reply and
        // then linger): wait for its exit without reaping, then kill it if it's still there.
        var info = siginfo_t()
        var exitedAlone = false
        while true {
            info = siginfo_t()
            let r = waitid(P_PID, id_t(child), &info, WEXITED | WNOHANG | WNOWAIT)
            if r == 0 && info.si_pid == child {
                exitedAlone = !lock.withLock { killed }
                break
            }
            if r != 0 && errno != EINTR { break } // no such child: reaped below, reported as failed
            if DispatchTime.now() >= deadline { lock.withLock { killLocked() } }
            usleep(5_000)
        }
        // Known to have exited: no kill may reach its pid from here on.
        lock.withLock { pid = 0 }
        wrote.wait() // the writer ends once the worker is gone (its read end closed)

        var status: Int32 = 0
        var usage = rusage()
        var reaped: pid_t
        repeat { reaped = wait4(child, &status, 0, &usage) } while reaped < 0 && errno == EINTR
        let clean = reaped == child && (status & 0x7f) == 0 && ((status >> 8) & 0xff) == 0
        if frame.code == UInt32(ReplyCode.ok.rawValue) && !(exitedAlone && clean) {
            // A good frame from a worker that lingered, was killed, or exited wrongly.
            frame = failed(.workerFailed)
        }
        #if DEBUG
        if frame.code == UInt32(ReplyCode.timeout.rawValue) {
            // For the tests: the reap.
            let report = "reaped=\(reaped == child ? child : -1)\nmaxrss=\(usage.ru_maxrss)"
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
