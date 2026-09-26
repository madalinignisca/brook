import AppKit

/// Whether Brook is the active app (its window can be seen and used).
enum AppActivity {
    @MainActor static var isActive: Bool { NSApp?.isActive ?? true }
}
