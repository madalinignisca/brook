import AppKit
import BrookCore
import SwiftUI

/// App quit during a call goes through the call's leave first (see QuitCoordinator).
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var quit: QuitCoordinator?

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        quit?.shouldTerminate() ?? .terminateNow
    }
}

@main
struct BrookApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var store: SessionStore
    @State private var form: LoginForm
    @State private var calls = CallCenter()

    init() {
        let store = SessionStore(persistence: .live())
        _store = State(initialValue: store)
        _form = State(initialValue: LoginForm(store: store))
    }

    var body: some Scene {
        Window("Brook", id: "main") {
            Group {
                switch store.phase {
                case let .signedIn(user):
                    if let client = store.client {
                        SignedInView(
                            user: user, client: client, calls: calls, signOut: { store.signOut() },
                            recoveryCodesLeft: store.recoveryCodesLeft)
                    }
                case .restoring:
                    ProgressView("Signing in…").frame(maxWidth: .infinity, maxHeight: .infinity)
                case .signedOut, .signingIn, .needsCode: LoginView(form: form)
                }
            }
            .task { await store.restoreAtLaunch() } // once per process; later appearances no-op
            .frame(minWidth: 380, minHeight: 480)
            .onAppear { appDelegate.quit = calls.quit }
            // Signed out (by the user or remotely): a call can't outlive its session. Its
            // own end path runs, and the call window closes itself once the call is gone.
            .onChange(of: store.phase) { _, phase in
                if case .signedIn = phase { return }
                Task { await calls.endAll() }
            }
        }
        .defaultSize(width: 720, height: 560)

        Window("Call", id: "call") {
            CallWindow(center: calls)
        }
        .defaultSize(width: 800, height: 560)
    }
}
