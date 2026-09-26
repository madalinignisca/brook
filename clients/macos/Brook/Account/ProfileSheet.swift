import BrookCore
import SwiftUI

struct ProfileSheet: View {
    @State private var model: ProfileModel
    let onSaved: (FfiUser) -> Void
    @Environment(\.dismiss) private var dismiss

    init(client: any AccountClient, onSaved: @escaping (FfiUser) -> Void) {
        _model = State(initialValue: ProfileModel(client: client))
        self.onSaved = onSaved
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Edit Profile").font(.title2)
            Form {
                TextField("Display name", text: $model.name)
                TextField("Status", text: $model.status, prompt: Text("What you're up to (optional)"))
            }
            .disabled(!model.loaded)
            if let error = model.error {
                Text(error).foregroundStyle(.red)
            } else if model.loaded, let problem = model.problem {
                Text(problem).font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Save") { Task { await model.save() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.canSave)
            }
        }
        .padding(20)
        .frame(width: 420)
        .task { await model.load() }
        .onChange(of: model.saved) { _, saved in
            if let saved { onSaved(saved); dismiss() }
        }
    }
}
