import BrookCore
import BrookMedia
import Foundation
import Observation

/// Owns the live call (at most one) and the quit handshake around it.
@MainActor
@Observable
final class CallCenter {
    private(set) var call: CallModel?
    private(set) var joining = false
    private(set) var joinError: String?
    let quit = QuitCoordinator()

    /// Permissions first (microphone, then camera), so a prompt's human time never eats into
    /// the engine's capture budget; then the engine, the join, and the attachment.
    func join(_ channel: FfiChannel, name: String, client: any FfiBrookClientProtocol) async {
        guard call == nil, !joining else { return }
        joining = true
        joinError = nil
        defer { joining = false }
        let plan = await JoinPlan.resolve(SystemAuthorization())
        let engine = WebRTCEngine(options: MediaOptions(
            audio: plan.microphone, video: plan.camera ? CameraCapture() : nil))
        do {
            let handle = try await client.joinCall(
                channelId: channel.id, engine: engine, publish: plan.publishes)
            engine.attach(handle)
            let model = CallModel(channelName: name, plan: plan, handle: handle, media: engine)
            await model.start()
            call = model
            quit.leaveActiveCall = { [weak self] in await self?.leave() }
        } catch {
            await engine.close()
            joinError = "Couldn't join the call."
        }
    }

    /// Leave and wait (bounded) for the engine to close; then the call is gone.
    func leave() async {
        guard let call else { return }
        await call.leave()
        self.call = nil
        quit.leaveActiveCall = nil
    }
}
