import BrookCore
import SwiftUI

struct ChangePasswordSheet: View {
    @State private var model: ChangePasswordModel
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient) {
        _model = State(initialValue: ChangePasswordModel(client: client))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Change Password").font(.title2)
            if let done = model.done {
                Text(done)
                HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
            } else {
                Form {
                    SecureField("Current password", text: $model.current)
                    SecureField("New password", text: $model.new)
                    SecureField("Confirm new password", text: $model.confirm)
                    Toggle("Sign out of other devices", isOn: $model.signOutOtherDevices)
                }
                if let error = model.error {
                    Text(error).foregroundStyle(.red)
                } else if let problem = model.problem, !model.new.isEmpty || !model.current.isEmpty {
                    Text(problem).font(.callout).foregroundStyle(.secondary)
                }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button("Change Password") { Task { await model.submit() } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.problem != nil || model.busy)
                }
            }
        }
        .padding(20)
        .frame(width: 420)
        .onDisappear { model.clear() }  // nothing keeps the passwords once the sheet is gone
    }
}

struct AdminResetSheet: View {
    @State private var model: AdminResetModel
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient, selfId: String) {
        _model = State(initialValue: AdminResetModel(client: client, selfId: selfId))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Reset a User's Password").font(.title2)
            if let done = model.done {
                Text(done)
                HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
            } else {
                Form {
                    Picker("User", selection: $model.selectedId) {
                        Text("Choose…").tag(String?.none)
                        ForEach(model.users, id: \.id) { user in
                            Text("\(user.displayName) (\(user.handle))").tag(Optional(user.id))
                        }
                    }
                    SecureField("New password", text: $model.new)
                    SecureField("Confirm new password", text: $model.confirm)
                    SecureField("Your own password", text: $model.adminPassword)
                }
                Text("They are signed out of every device. Admins change their own passwords.")
                    .font(.callout).foregroundStyle(.secondary)
                if let error = model.error {
                    Text(error).foregroundStyle(.red)
                }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button("Reset Password") { Task { await model.submit() } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.problem != nil || model.busy)
                }
            }
        }
        .padding(20)
        .frame(width: 460)
        .task { await model.load() }
        .onDisappear { model.clear() }
    }
}

struct AddUserSheet: View {
    @State private var model: AddUserModel
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient) {
        _model = State(initialValue: AddUserModel(client: client))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add User").font(.title2)
            if let done = model.done {
                Text(done)
                HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
            } else {
                Form {
                    // No content types: nothing here should be offered to AutoFill as a login.
                    TextField("Handle", text: $model.handle)
                        .autocorrectionDisabled()
                    TextField("Display name", text: $model.displayName)
                    SecureField("Password", text: $model.password)
                    SecureField("Confirm password", text: $model.confirm)
                    Button("Generate") { model.generate() }
                    if let generated = model.generated {
                        // Shown once, until the Password field is edited or the sheet closes.
                        Text(generated)
                            .font(.system(.body, design: .monospaced))
                            .textSelection(.enabled)
                    }
                    SecureField("Your password", text: $model.adminPassword)
                }
                Text("The new account is a member. Give them the password; they can change it under Change Password.")
                    .font(.callout).foregroundStyle(.secondary)
                if let error = model.error {
                    Text(error).foregroundStyle(.red)
                } else if let problem = model.problem {
                    Text(problem).font(.callout).foregroundStyle(.secondary)
                }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button("Add User") { Task { await model.submit() } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.problem != nil || model.busy)
                }
            }
        }
        .padding(20)
        .frame(width: 460)
        .onDisappear { model.clear() }  // nothing keeps the passwords once the sheet is gone
    }
}
