import BrookMedia
@preconcurrency import ScreenCaptureKit

/// The system screen/window picker. The user's pick is the consent: no screen-recording
/// entitlement or prompt is involved. Returns nil when cancelled or unavailable.
@MainActor
final class ScreenPicker: NSObject, SCContentSharingPickerObserver {
    /// The picked filter crosses from the picker's callback to the caller exactly once.
    private struct Picked: @unchecked Sendable { let filter: SCContentFilter? }
    private var pending: CheckedContinuation<Picked, Never>?

    func pick() async -> VideoCapture? {
        guard pending == nil else { return nil }
        let picker = SCContentSharingPicker.shared
        var config = SCContentSharingPickerConfiguration()
        config.allowedPickerModes = [.singleDisplay, .singleWindow]
        picker.defaultConfiguration = config
        picker.add(self)
        picker.isActive = true
        let picked = await withCheckedContinuation { continuation in
            pending = continuation
            picker.present()
        }
        return picked.filter.map { ScreenCapture(filter: $0) }
    }

    /// The call went away while the picker was open: close it, answer nil.
    func cancel() {
        guard pending != nil else { return }
        finish(nil)
    }

    private func finish(_ filter: SCContentFilter?) {
        let picker = SCContentSharingPicker.shared
        picker.remove(self)
        picker.isActive = false
        pending?.resume(returning: Picked(filter: filter))
        pending = nil
    }

    nonisolated func contentSharingPicker(
        _ picker: SCContentSharingPicker, didUpdateWith filter: SCContentFilter, for stream: SCStream?
    ) {
        nonisolated(unsafe) let filter = filter
        Task { @MainActor in self.finish(filter) }
    }

    nonisolated func contentSharingPicker(_ picker: SCContentSharingPicker, didCancelFor stream: SCStream?) {
        Task { @MainActor in self.finish(nil) }
    }

    nonisolated func contentSharingPickerStartDidFailWithError(_ error: any Error) {
        Task { @MainActor in self.finish(nil) }
    }
}
