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
    /// Bumped by `endAll` (sign-out): a join that finishes into an older generation is left at
    /// once instead of becoming the live call.
    private var generation = 0

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
    func join(channelId: String, name: String, client: any FfiBrookClientProtocol) async {
        guard call == nil, joinTask == nil else { return }
        let mine = generation // when the join is accepted, not when its work starts
        let task = Task { await self.performJoin(channelId, name: name, client: client, generation: mine) }
        joinTask = task
        quit.leaveActiveCall = { [weak self] in await self?.shutdown() }
        await task.value
        joinTask = nil
        if call == nil { quit.leaveActiveCall = nil }
    }

    private func performJoin(
        _ channelId: String, name: String, client: any FfiBrookClientProtocol, generation mine: Int
    ) async {
        joining = true
        joinError = nil
        defer { joining = false }
        let plan = await JoinPlan.resolve(auth)
        let engine = makeEngine(plan)
        do {
            let handle = try await client.joinCall(
                channelId: channelId, engine: engine, publish: plan.publishes)
            engine.attach(handle)
            let model = CallModel(channelName: name, plan: plan, handle: handle, media: engine)
            guard mine == generation else {
                await model.leave() // signed out while joining: this call has no session
                return
            }
            await model.start()
            guard mine == generation else {
                await model.leave()
                return
            }
            call = model
        } catch {
            await engine.close()
            if mine == generation { joinError = "Couldn't join the call." }
        }
    }

    /// Quit: let a join in flight finish, then leave.
    private func shutdown() async {
        await joinTask?.value
        await leave()
    }

    /// Signed out: a join still in flight is abandoned when it finishes, and a live call is left.
    func endAll() async {
        generation += 1
        joinError = nil
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
