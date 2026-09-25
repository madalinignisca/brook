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
        let store = SessionStore()
        _store = State(initialValue: store)
        _form = State(initialValue: LoginForm(store: store))
    }

    var body: some Scene {
        Window("Brook", id: "main") {
            Group {
                switch store.phase {
                case let .signedIn(user):
                    if let client = store.client {
                        SignedInView(user: user, client: client, calls: calls)
                    }
                case .signedOut, .signingIn: LoginView(form: form)
                }
            }
            .frame(minWidth: 380, minHeight: 480)
            .onAppear { appDelegate.quit = calls.quit }
        }
        .defaultSize(width: 720, height: 560)

        Window("Call", id: "call") {
            CallWindow(center: calls)
        }
        .defaultSize(width: 800, height: 560)
    }
}
