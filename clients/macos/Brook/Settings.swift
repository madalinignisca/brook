import Foundation

/// Where the login form's configuration comes from. The server address is what the user
/// typed, remembered across launches (there is deliberately no environment override: it
/// would silently replace the saved choice). The plain-http opt-in matches the GNOME
/// client's `BROOK_ALLOW_INSECURE_HTTP=1`, with a hidden default as the Finder-launch
/// equivalent, since an app opened from Finder has no environment.
struct Settings {
    static let lastServerKey = "LastServer"
    /// Hidden, no UI: `defaults write dev.brook.Brook AllowInsecureHTTP -bool YES`.
    static let allowInsecureKey = "AllowInsecureHTTP"
    static let fallbackServer = "https://localhost"

    let defaults: UserDefaults
    let environment: [String: String]

    init(defaults: UserDefaults = .standard, environment: [String: String] = ProcessInfo.processInfo.environment) {
        self.defaults = defaults
        self.environment = environment
    }

    var serverPrefill: String {
        defaults.string(forKey: Self.lastServerKey) ?? Self.fallbackServer
    }

    /// Plain http to any host: the password travels unencrypted. Dev/testing only.
    var allowInsecureHTTP: Bool {
        environment["BROOK_ALLOW_INSECURE_HTTP"] == "1" || defaults.bool(forKey: Self.allowInsecureKey)
    }

    /// The server of the last successful sign-in, if any (the launch restore's server).
    var lastGoodServer: String? { defaults.string(forKey: Self.lastServerKey) }

    /// Called only after a successful login, so a mistyped address is never remembered.
    func saveLastGoodServer(_ server: String) {
        defaults.set(server, forKey: Self.lastServerKey)
    }
}
