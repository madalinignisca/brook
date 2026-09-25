import Foundation

/// Validates what the user typed as the server address, before it reaches the core or
/// is remembered. Only a bare address is accepted: credentials in the URL would end up
/// persisted as the "last server", and a query or fragment has no meaning for the API.
enum ServerAddress {
    enum Problem: Error, Equatable {
        case invalid
        case notJustAnAddress
    }

    static func parse(_ input: String) -> Result<String, Problem> {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = URLComponents(string: trimmed),
              let scheme = url.scheme?.lowercased(), ["http", "https"].contains(scheme),
              let host = url.host, !host.isEmpty
        else { return .failure(.invalid) }
        guard url.user == nil, url.password == nil, url.query == nil, url.fragment == nil
        else { return .failure(.notJustAnAddress) }
        return .success(trimmed)
    }
}
