import AppKit

/// AppKit's opener, kept apart so the model file stays free of AppKit.
enum NSWorkspaceBridge {
    @MainActor static func open(_ url: URL) -> Bool { NSWorkspace.shared.open(url) }
}
