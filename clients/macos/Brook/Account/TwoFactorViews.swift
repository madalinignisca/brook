import AppKit
import BrookCore
import SwiftUI
import UniformTypeIdentifiers

struct TwoFactorSetupSheet: View {
    @State private var model: TwoFactorSetupModel
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient) {
        _model = State(initialValue: TwoFactorSetupModel(client: client))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Turn On Two-Factor Sign-In").font(.title2)
            switch model.step {
            case .password:
                Text("Signing in will need a code from an authenticator app on your phone, as well as your password.")
                    .foregroundStyle(.secondary)
                Form { SecureField("Your password", text: $model.password) }
                    .onSubmit { Task { await model.start() } }
                buttons(primary: "Continue", enabled: !model.password.isEmpty) { await model.start() }
            case .scan:
                Text("Scan this code with your authenticator app, or enter the key by hand.")
                    .foregroundStyle(.secondary)
                if let uri = model.uri, let qr = QRCode.image(for: uri, size: 180) {
                    Image(nsImage: qr).interpolation(.none).frame(width: 180, height: 180)
                        .frame(maxWidth: .infinity)
                }
                if let key = model.key {
                    Text(key).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                        .frame(maxWidth: .infinity)
                }
                Form {
                    TextField("Code from the app", text: $model.code, prompt: Text("123 456"))
                        .textContentType(.oneTimeCode)
                }
                .onSubmit { Task { await model.activate() } }
                buttons(primary: "Turn On", enabled: !model.code.isEmpty) { await model.activate() }
            case let .codes(codes):
                Text("Save these recovery codes somewhere safe. Each one signs you in once if you lose your phone. They won't be shown again.")
                    .foregroundStyle(.secondary)
                RecoveryCodesList(codes: codes)
                Toggle("I've saved these codes", isOn: $model.savedCodes)
                HStack {
                    Spacer()
                    Button("Done") { model.finish() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(!model.savedCodes)
                }
            case .done:
                Text(model.note ?? "")
                HStack { Spacer(); Button("Close") { dismiss() }.keyboardShortcut(.defaultAction) }
            }
            if let error = model.error { Text(error).foregroundStyle(.red) }
        }
        .padding(20)
        .frame(width: 440)
        .onDisappear { model.clear() } // the password, the secret and the codes go with the sheet
    }

    private func buttons(primary: String, enabled: Bool, action: @escaping () async -> Void) -> some View {
        HStack {
            Spacer()
            Button("Cancel", role: .cancel) { dismiss() }
            Button(primary) { Task { await action() } }
                .keyboardShortcut(.defaultAction)
                .disabled(!enabled || model.busy)
        }
    }
}

/// Recovery codes with Copy and Save…; they are never written anywhere without the user choosing.
struct RecoveryCodesList: View {
    let codes: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(codes.joined(separator: "\n"))
                .font(.system(.body, design: .monospaced))
                .textSelection(.enabled)
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 6))
            HStack {
                Button("Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(codes.joined(separator: "\n"), forType: .string)
                }
                Button("Save…") { save() }
            }
        }
    }

    private func save() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "Brook recovery codes.txt"
        panel.allowedContentTypes = [.plainText]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let text = codes.joined(separator: "\n") + "\n"
        // Readable by the user only.
        FileManager.default.createFile(
            atPath: url.path, contents: Data(text.utf8), attributes: [.posixPermissions: 0o600])
    }
}

struct SecondFactorSheet: View {
    @State private var model: SecondFactorModel
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient, action: SecondFactorModel.Action) {
        _model = State(initialValue: SecondFactorModel(client: client, action: action))
    }

    private var title: String {
        model.action == .turnOff ? "Turn Off Two-Factor Sign-In" : "New Recovery Codes"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.title2)
            if model.done {
                if let codes = model.newCodes {
                    Text("Your old recovery codes no longer work. Save these; they won't be shown again.")
                        .foregroundStyle(.secondary)
                    RecoveryCodesList(codes: codes)
                } else {
                    Text("Two-factor sign-in is off. Signing in needs only your password.")
                }
                HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
            } else {
                Form {
                    SecureField("Your password", text: $model.password)
                    if model.useRecovery {
                        TextField("Recovery code", text: $model.code).autocorrectionDisabled()
                    } else {
                        TextField("Code from the app", text: $model.code, prompt: Text("123 456"))
                            .textContentType(.oneTimeCode)
                    }
                }
                Button(model.useRecovery ? "Use the authenticator code instead" : "Use a recovery code instead") {
                    model.useRecovery.toggle()
                    model.code = ""
                }
                .buttonStyle(.link)
                if let error = model.error { Text(error).foregroundStyle(.red) }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button(model.action == .turnOff ? "Turn Off" : "Replace Codes") {
                        Task { await model.submit() }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(model.problem != nil || model.busy)
                }
            }
        }
        .padding(20)
        .frame(width: 420)
        .onDisappear { model.dismissed() } // the password, the code and any new codes
    }
}

struct AdminTotpResetSheet: View {
    @State private var model: AdminTotpResetModel
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient, selfId: String) {
        _model = State(initialValue: AdminTotpResetModel(client: client, selfId: selfId))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Reset a User's Two-Factor Sign-In").font(.title2)
            if let done = model.done {
                Text(done)
                HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
            } else {
                Text("For someone who lost their phone and their recovery codes. They can sign in with their password and turn it on again.")
                    .foregroundStyle(.secondary)
                Form {
                    Picker("User", selection: $model.selectedId) {
                        Text("Choose…").tag(String?.none)
                        ForEach(model.users, id: \.id) { Text($0.displayName).tag(Optional($0.id)) }
                    }
                    SecureField("Your own password", text: $model.adminPassword)
                }
                if let error = model.error { Text(error).foregroundStyle(.red) }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button("Reset") { Task { await model.submit() } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.problem != nil || model.busy)
                }
            }
        }
        .padding(20)
        .frame(width: 420)
        .task { await model.load() }
        .onDisappear { model.clear() }
    }
}
