#if DEBUG
import Darwin
import Foundation

/// What this worker's sandbox and limits allow, one `key=value` per line (Debug only; the
/// broker's tests read it). Every value that should be refused names the errno it got.
func probeReport() -> String {
    var lines = ["pid=\(getpid())", "ppid=\(getppid())"]

    // The network: a public address.
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port = in_port_t(443).bigEndian
    addr.sin_addr.s_addr = inet_addr("1.1.1.1")
    let rc = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    lines.append("connect=\(rc == 0 ? 0 : errno)")
    close(fd)

    // User data: a write under the real home (not the container's).
    let home = getpwuid(getuid()).map { String(cString: $0.pointee.pw_dir) } ?? "/Users"
    let wfd = open(home + "/.brook-probe-\(getpid())", O_CREAT | O_WRONLY | O_EXCL, 0o600)
    lines.append("writeHome=\(wfd >= 0 ? 0 : errno)")
    if wfd >= 0 { close(wfd); unlink(home + "/.brook-probe-\(getpid())") }

    // No new processes.
    // Swift hides fork(); reach it the way C would, since it's the call to prove refused.
    typealias Fork = @convention(c) () -> pid_t
    let forkFn = unsafeBitCast(dlsym(UnsafeMutableRawPointer(bitPattern: -2), "fork"), to: Fork.self)
    let child = forkFn()
    if child == 0 { _exit(0) }
    lines.append("fork=\(child >= 0 ? 0 : errno)")
    var spawned: pid_t = 0
    let argv: [UnsafeMutablePointer<CChar>?] = [strdup("/usr/bin/true"), nil]
    let src = posix_spawn(&spawned, "/usr/bin/true", nil, nil, argv, nil)
    lines.append("spawn=\(src)")

    // Only stdin, stdout and stderr are open.
    var open: [Int32] = []
    for f in Int32(0) ..< 256 where fcntl(f, F_GETFD) != -1 { open.append(f) }
    lines.append("fds=\(open.map(String.init).joined(separator: ","))")

    // The limits, hard and soft.
    for (name, r) in [("cpu", RLIMIT_CPU), ("nproc", RLIMIT_NPROC)] {
        var l = rlimit()
        getrlimit(r, &l)
        lines.append("\(name)=\(l.rlim_cur)/\(l.rlim_max)")
    }

    // The broker's service, as a client: it must refuse.
    let c = NSXPCConnection(serviceName: imageDecoderServiceName)
    c.remoteObjectInterface = NSXPCInterface(with: ImageDecoding.self)
    c.resume()
    let done = DispatchSemaphore(value: 0)
    var reached = "no"
    let proxy = c.remoteObjectProxyWithErrorHandler { _ in done.signal() } as? ImageDecoding
    proxy?.decode(Data([0x89]), kind: 1) { code, _, _, _ in reached = "code\(code)"; done.signal() }
    _ = done.wait(timeout: .now() + 2)
    c.invalidate()
    lines.append("brokerClient=\(reached)")

    return lines.joined(separator: "\n")
}
#endif
