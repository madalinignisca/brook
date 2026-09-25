import SwiftUI

struct LoginView: View {
    @Bindable var form: LoginForm

    var body: some View {
        if form.needsCode {
            CodeStepView(form: form)
        } else {
            passwordStep
        }
    }

    private var passwordStep: some View {
        VStack(spacing: 20) {
            Text("Welcome to Brook")
                .font(.largeTitle.weight(.semibold))

            Form {
                TextField("Server", text: $form.server, prompt: Text("https://chat.example.com"))
                    .textContentType(.URL)
                TextField("Handle", text: $form.handle)
                    .textContentType(.username)
                SecureField("Password", text: $form.password)
                    .textContentType(.password)
            }
            .formStyle(.grouped)
            // A grouped Form grows to fill the window; size it to its rows instead so the
            // button and messages sit right under the fields.
            .scrollDisabled(true)
            .fixedSize(horizontal: false, vertical: true)
            .disabled(form.isBusy)
            .onSubmit(submit)

            HStack(spacing: 8) {
                if form.isBusy { ProgressView().controlSize(.small) }
                Button("Log In", action: submit)
                    .keyboardShortcut(.defaultAction)
                    .controlSize(.large)
                    .disabled(form.isBusy)
            }

            if let error = form.error {
                Text(error)
                    .foregroundStyle(.red)
                    .multilineTextAlignment(.center)
                    .textSelection(.enabled)
            }
            if let warning = form.store.signOutWarning {
                Label(warning, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .multilineTextAlignment(.center)
            }
            if let warning = form.insecureWarning {
                Label(warning, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
            }
        }
        .padding(24)
        .frame(maxWidth: 420, maxHeight: .infinity)
    }

    private func submit() {
        Task { await form.submit() }
    }
}

/// The second sign-in step for an account with two-factor sign-in on.
struct CodeStepView: View {
    @Bindable var form: LoginForm

    var body: some View {
        VStack(spacing: 20) {
            Text("Two-Factor Sign-In")
                .font(.largeTitle.weight(.semibold))
            Text(form.useRecovery
                 ? "Enter one of the recovery codes you saved when you turned it on."
                 : "Enter the 6-digit code from your authenticator app.")
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)

            Form {
                if form.useRecovery {
                    TextField("Recovery code", text: $form.code, prompt: Text("xxxx-xxxx-xxxx-xxxx-xxxx"))
                        .autocorrectionDisabled()
                } else {
                    // One-time-code content type: the system can fill it from the Passwords app.
                    TextField("Code", text: $form.code, prompt: Text("123 456"))
                        .textContentType(.oneTimeCode)
                }
            }
            .formStyle(.grouped)
            .scrollDisabled(true)
            .fixedSize(horizontal: false, vertical: true)
            .disabled(form.isBusy)
            .onSubmit(submit)

            HStack(spacing: 8) {
                Button("Back") { form.back() }
                    .disabled(form.isBusy)
                if form.isBusy { ProgressView().controlSize(.small) }
                Button("Verify", action: submit)
                    .keyboardShortcut(.defaultAction)
                    .controlSize(.large)
                    .disabled(form.isBusy || form.code.trimmingCharacters(in: .whitespaces).isEmpty)
            }

            Button(form.useRecovery ? "Use the authenticator code instead" : "Use a recovery code instead") {
                form.useRecovery.toggle()
                form.code = ""
            }
            .buttonStyle(.link)

            if let error = form.error {
                Text(error)
                    .foregroundStyle(.red)
                    .multilineTextAlignment(.center)
            }
        }
        .padding(24)
        .frame(maxWidth: 420, maxHeight: .infinity)
    }

    private func submit() {
        Task { await form.submitCode() }
    }
}
