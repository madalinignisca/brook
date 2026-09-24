import BrookCore
import BrookMedia
import Foundation
import Observation

/// Owns the live call (at most one) and the quit handshake around it.
@MainActor
@Observable
final class CallCenter {
    typealias EngineFactory = @MainActor (JoinPlan) -> any CallEngine

    private(set) var call: CallModel?
    private(set) var joining = false
    private(set) var joinError: String?
    let quit: QuitCoordinator
    private let auth: AuthorizationSource
    private let makeEngine: EngineFactory
    private var joinTask: Task<Void, Never>?

    init(
        auth: AuthorizationSource = SystemAuthorization(),
        quit: QuitCoordinator = QuitCoordinator(),
        makeEngine: @escaping EngineFactory = CallCenter.liveEngine
    ) {
        self.auth = auth
        self.quit = quit
        self.makeEngine = makeEngine
    }

    static func liveEngine(_ plan: JoinPlan) -> any CallEngine {
        WebRTCEngine(options: MediaOptions(
            audio: plan.microphone, video: plan.camera ? CameraCapture() : nil))
    }

    /// Permissions first (microphone, then camera), so a prompt's human time never eats into
    /// the engine's capture budget; then the engine, the join, and the attachment. Quit is
    /// handled from the start: core may already be publishing before join_call returns.
    func join(_ channel: FfiChannel, name: String, client: any FfiBrookClientProtocol) async {
        guard call == nil, joinTask == nil else { return }
        let task = Task { await self.performJoin(channel, name: name, client: client) }
        joinTask = task
        quit.leaveActiveCall = { [weak self] in await self?.shutdown() }
        await task.value
        joinTask = nil
        if call == nil { quit.leaveActiveCall = nil }
    }

    private func performJoin(
        _ channel: FfiChannel, name: String, client: any FfiBrookClientProtocol
    ) async {
        joining = true
        joinError = nil
        defer { joining = false }
        let plan = await JoinPlan.resolve(auth)
        let engine = makeEngine(plan)
        do {
            let handle = try await client.joinCall(
                channelId: channel.id, engine: engine, publish: plan.publishes)
            engine.attach(handle)
            let model = CallModel(channelName: name, plan: plan, handle: handle, media: engine)
            await model.start()
            call = model
        } catch {
            await engine.close()
            joinError = "Couldn't join the call."
        }
    }

    /// Quit: let a join in flight finish, then leave.
    private func shutdown() async {
        await joinTask?.value
        await leave()
    }

    /// Leave and wait (bounded) for the engine to close; then the call is gone.
    func leave() async {
        guard let call else { return }
        await call.leave()
        if self.call === call { self.call = nil }
        quit.leaveActiveCall = nil
    }
}
