import SwiftUI

struct LoginView: View {
    @Bindable var form: LoginForm

    var body: some View {
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
            if let warning = form.insecureWarning {
                Label(warning, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
            }
        }
        .padding(24)
        .frame(maxWidth: 420)
    }

    private func submit() {
        Task { await form.submit() }
    }
}
