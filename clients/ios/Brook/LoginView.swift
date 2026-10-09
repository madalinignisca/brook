// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import SwiftUI

/// The sign-in screen: a plain system `Form`, nothing branded. It adds no logic of its own;
/// the checks, the messages and the insecure-http rule are all in `LoginForm` and `SessionStore`
/// (shared with the Mac, and covered by their tests).
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
        NavigationStack {
            Form {
                Section {
                    // Empty on a first launch: the grey prompt shows the shape of an address
                    // without pretending to be one (see `ThisDevice.fallbackServer`).
                    TextField("Server", text: $form.server, prompt: Text("https://chat.example.com"))
                        .textContentType(.URL)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Handle", text: $form.handle)
                        .textContentType(.username)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField("Password", text: $form.password)
                        .textContentType(.password)
                }
                .disabled(form.isBusy)

                Section {
                    Button(action: submit) {
                        HStack {
                            Text("Log In")
                            if form.isBusy {
                                Spacer()
                                ProgressView()
                            }
                        }
                    }
                    .disabled(form.isBusy)
                }

                messages
            }
            .onSubmit(submit)
            .navigationTitle("Brook")
        }
    }

    /// Everything the form has to say, in one section that exists only when there is something.
    @ViewBuilder private var messages: some View {
        let error = form.error
        let warning = form.store.signOutWarning
        let insecure = form.insecureWarning
        if error != nil || warning != nil || insecure != nil {
            Section {
                if let error {
                    Text(error)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }
                if let warning {
                    Label(warning, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                }
                if let insecure {
                    Label(insecure, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                }
            }
        }
    }

    private func submit() {
        Task { await form.submit() }
    }
}

/// The second sign-in step, for an account with two-factor sign-in on.
struct CodeStepView: View {
    @Bindable var form: LoginForm

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    if form.useRecovery {
                        TextField("Recovery code", text: $form.code, prompt: Text("xxxx-xxxx-xxxx-xxxx-xxxx"))
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    } else {
                        // One-time-code content type: the keyboard offers the code from the
                        // Passwords app or a message.
                        TextField("Code", text: $form.code, prompt: Text("123 456"))
                            .textContentType(.oneTimeCode)
                            .keyboardType(.numberPad)
                    }
                } footer: {
                    Text(form.useRecovery
                         ? "Enter one of the recovery codes you saved when you turned it on."
                         : "Enter the 6-digit code from your authenticator app.")
                }
                .disabled(form.isBusy)

                Section {
                    Toggle("Use a recovery code", isOn: $form.useRecovery)
                        .onChange(of: form.useRecovery) { form.code = "" }
                }
                .disabled(form.isBusy)

                Section {
                    Button(action: submit) {
                        HStack {
                            Text("Verify")
                            if form.isBusy {
                                Spacer()
                                ProgressView()
                            }
                        }
                    }
                    .disabled(form.isBusy || form.code.trimmingCharacters(in: .whitespaces).isEmpty)
                }

                if let error = form.error {
                    Section { Text(error).foregroundStyle(.red) }
                }
            }
            .onSubmit(submit)
            .navigationTitle("Two-Factor Sign-In")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Back") { form.back() }.disabled(form.isBusy)
                }
            }
        }
    }

    private func submit() {
        Task { await form.submitCode() }
    }
}
