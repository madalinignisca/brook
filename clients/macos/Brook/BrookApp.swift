// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit
import BrookCore
import SwiftUI

/// App quit during a call goes through the call's leave first (see QuitCoordinator).
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var quit: QuitCoordinator?

    func applicationDidFinishLaunching(_ notification: Notification) {
        MacNotifier.shared.install() // clicks on notifications arrive through it
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        quit?.shouldTerminate() ?? .terminateNow
    }
}

@main
struct BrookApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var store: SessionStore
    @State private var form: LoginForm
    @State private var about: AboutModel
    @State private var calls = CallCenter()

    init() {
        // Staged files don't outlive the app, so a sole instance's earlier copies are all stale; a
        // second instance leaves them alone (the first may still hold a staged draft).
        // Judged by process id, not by a count: whether this process is listed yet at this point is not
        // something to rely on.
        let others = NSRunningApplication.runningApplications(withBundleIdentifier: Bundle.main.bundleIdentifier ?? "")
            .filter { $0.processIdentifier != getpid() }
        if others.isEmpty { DropImport.sweep(olderThan: 0) }
        let store = SessionStore(persistence: .live(), makeFeed: SessionStore.macFeed)
        _store = State(initialValue: store)
        let form = LoginForm(store: store)
        _form = State(initialValue: form)
        // The closures read the live store and form at each refresh, which a property
        // initialiser could not reach.
        _about = State(initialValue: AboutModel(
            allowInsecureHTTP: { store.settings.allowInsecureHTTP },
            target: {
                AboutModel.target(
                    signedInServer: store.server, typed: form.server,
                    remembered: store.settings.lastGoodServer)
            }))
    }

    var body: some Scene {
        Window(AppTitle.main, id: "main") {
            Group {
                switch store.phase {
                case let .signedIn(user):
                    if let client = store.client {
                        SignedInView(
                            user: user, client: client, calls: calls, signOut: { store.signOut() },
                            recoveryCodesLeft: store.recoveryCodesLeft, feed: store.feed as? CacheFeed,
                            offersRemoval: store.offersRemoval,
                            signOutChoosing: { store.signOut(removeData: $0) })
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
        .commands { CommandGroup(replacing: .appInfo) { AboutCommand(model: about) } }

        Window("About Brook", id: "about") {
            AboutView(model: about)
        }
        .windowResizability(.contentSize)
        // A restored About would reopen at launch for whatever server is current then.
        .restorationBehavior(.disabled)
        // Keeps this scene's own item out of the Window menu: the app menu's About is the way in.
        .commandsRemoved()

        Window("Call", id: "call") {
            CallWindow(center: calls)
        }
        .defaultSize(width: 800, height: 560)

        // Named in full: the app has its own `Settings` (the login form's configuration).
        SwiftUI.Settings {
            SettingsView()
        }
    }
}
