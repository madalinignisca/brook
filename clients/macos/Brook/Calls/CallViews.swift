import AppKit
import BrookCore
import BrookMedia
import SwiftUI
@preconcurrency import WebRTC

/// A remote or local video track in a Metal view. Renderers are attached to the track object the
/// engine keeps alive (its wrapper's dealloc would detach them).
struct VideoTile: NSViewRepresentable {
    let track: RTCVideoTrack?

    final class Coordinator {
        var attached: RTCVideoTrack?
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> RTCMTLNSVideoView {
        RTCMTLNSVideoView(frame: .zero)
    }

    func updateNSView(_ view: RTCMTLNSVideoView, context: Context) {
        guard context.coordinator.attached !== track else { return }
        context.coordinator.attached?.remove(view)
        track?.add(view)
        context.coordinator.attached = track
    }

    static func dismantleNSView(_ view: RTCMTLNSVideoView, coordinator: Coordinator) {
        coordinator.attached?.remove(view)
        coordinator.attached = nil
    }
}

struct TileView: View {
    let tile: CallModel.Tile

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            Rectangle().fill(.black)
            if tile.video, tile.track != nil {
                VideoTile(track: tile.track)
            } else {
                Image(systemName: "person.crop.circle.fill")
                    .font(.system(size: 48))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            HStack(spacing: 4) {
                if !tile.audio { Image(systemName: "mic.slash.fill") }
                Text(tile.name)
            }
            .font(.callout)
            .padding(6)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 6))
            .padding(8)
        }
        .aspectRatio(16 / 9, contentMode: .fit)
        .clipShape(RoundedRectangle(cornerRadius: 10))
        // The tile is always black: its icon and name chip use dark-scheme colours in light mode too.
        .environment(\.colorScheme, .dark)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(tile.name)
    }
}

struct CallView: View {
    let call: CallModel
    var pickScreen: () async -> VideoCapture? = { nil }
    let leave: () async -> Void

    var body: some View {
        VStack(spacing: 12) {
            if let banner = call.banner {
                Text(banner)
                    .padding(8)
                    .frame(maxWidth: .infinity)
                    .background(.yellow.opacity(0.25), in: RoundedRectangle(cornerRadius: 8))
            }
            if let explanation = call.plan.explanation {
                Text(explanation).font(.callout).foregroundStyle(.secondary)
            }
            if call.sharing {
                Label("You're sharing your screen", systemImage: "rectangle.on.rectangle")
                    .font(.callout).foregroundStyle(.green)
            }
            if let error = call.shareError {
                Text(error).font(.callout).foregroundStyle(.red)
            }
            ScrollView {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 260), spacing: 10)], spacing: 10) {
                    ForEach(call.tiles) { TileView(tile: $0) }
                }
            }
            HStack(spacing: 16) {
                Button {
                    Task { await call.toggleMic() }
                } label: {
                    Label(call.micOn ? "Mute" : "Unmute",
                          systemImage: call.micOn ? "mic.fill" : "mic.slash.fill")
                }
                .disabled(!call.plan.microphone || call.isEnded)
                .help(JoinPlan.muteTooltip)
                .keyboardShortcut("m", modifiers: [.command, .shift])

                Button {
                    Task { await call.toggleCamera() }
                } label: {
                    Label(call.cameraOn ? "Stop camera" : "Start camera",
                          systemImage: call.cameraOn ? "video.fill" : "video.slash.fill")
                }
                .disabled(!call.plan.camera || call.isEnded)
                .keyboardShortcut("v", modifiers: [.command, .shift])

                Button {
                    Task {
                        if call.sharing {
                            await call.stopSharing()
                        } else if let capture = await pickScreen() {
                            await call.shareScreen(capture)
                        }
                    }
                } label: {
                    Label(call.sharing ? "Stop sharing" : "Share screen",
                          systemImage: call.sharing ? "rectangle.on.rectangle.slash" : "rectangle.on.rectangle")
                }
                .disabled(!call.plan.publishes || call.isEnded || call.sharingBusy)
                .keyboardShortcut("s", modifiers: [.command, .shift])

                Button(role: .destructive) {
                    Task { await leave() }
                } label: {
                    Label(call.isEnded ? "Close" : "Leave", systemImage: "phone.down.fill")
                }
                .disabled(call.leaving)
                .keyboardShortcut("w", modifiers: .command)
            }
            .labelStyle(.titleAndIcon)
        }
        .padding()
        .navigationTitle(call.channelName)
    }
}

/// The call window. While a call is live its close button is disabled: leaving (⌘W or Leave)
/// is the only way out, so closing always awaits `leave()` and the engine's `closed` first.
/// Without a call it closes normally, and the window closes itself once the call is gone.
struct CallWindow: View {
    let center: CallCenter
    /// One picker per request: a cancelled request's late callback can only reach its own
    /// (finished) picker, never the next request's.
    @State private var picker: ScreenPicker?
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        Group {
            if let call = center.call {
                CallView(call: call, pickScreen: {
                    let fresh = ScreenPicker()
                    picker = fresh
                    return await fresh.pick()
                }) {
                    await center.leave()
                }
            } else if center.joining {
                ProgressView("Joining…")
            } else {
                ContentUnavailableView("No call", systemImage: "phone")
            }
        }
        .frame(minWidth: 560, minHeight: 420)
        .background(WindowCloseControl(disabled: center.call != nil))
        // Dismissing goes through the close button, so it must be enabled first (the control
        // above updates in the same pass); dismiss on the next turn of the run loop.
        .onChange(of: center.call == nil) { _, gone in
            if gone {
                picker?.cancel()  // a picker left open would keep its observer registered
                DispatchQueue.main.async { dismissWindow(id: "call") }
            }
        }
        .onChange(of: center.call?.isEnded ?? false) { _, ended in
            if ended { picker?.cancel() }
        }
    }
}

struct WindowCloseControl: NSViewRepresentable {
    let disabled: Bool

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        let disabled = disabled
        DispatchQueue.main.async {
            view.window?.standardWindowButton(.closeButton)?.isEnabled = !disabled
        }
    }
}
