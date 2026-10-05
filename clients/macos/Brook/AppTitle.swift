/// The main window's title. A Debug build says so, so it can't be mistaken for the Release app
/// (which alone keeps the keychain group, and so alone stays signed in across launches).
enum AppTitle {
    static func window(debug: Bool) -> String {
        debug ? "Brook (Debug)" : "Brook"
    }

    #if DEBUG
        static let main = window(debug: true)
    #else
        static let main = window(debug: false)
    #endif
}
