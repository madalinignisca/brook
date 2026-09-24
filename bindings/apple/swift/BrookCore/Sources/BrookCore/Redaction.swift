import BrookCoreGenerated

// Tokens must never reach a log, crash report or the debugger's variable view.
//
// `description` alone is not enough: `dump()` and the debugger walk a value's stored
// properties through reflection, independently of its description. So every type that
// carries a token redacts all three: description, debugDescription and its Mirror.

extension FfiSession: CustomStringConvertible, CustomDebugStringConvertible,
    CustomReflectable
{
    public var description: String {
        "FfiSession(user: \(user.handle), accessToken: <redacted>, refreshToken: <redacted>)"
    }

    public var debugDescription: String { description }

    public var customMirror: Mirror {
        Mirror(
            self,
            children: [
                "user": user,
                "accessToken": "<redacted>",
                "refreshToken": "<redacted>",
            ],
            displayStyle: .struct
        )
    }
}

extension LoginResult: CustomStringConvertible, CustomDebugStringConvertible,
    CustomReflectable
{
    public var description: String {
        switch self {
        case let .loggedIn(session): "loggedIn(\(session))"
        }
    }

    public var debugDescription: String { description }

    // The child is the session itself, whose own Mirror redacts the tokens.
    public var customMirror: Mirror {
        switch self {
        case let .loggedIn(session):
            Mirror(self, children: ["loggedIn": session], displayStyle: .enum)
        }
    }
}
