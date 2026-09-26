import BrookCore
import SwiftUI

/// "Leave <channel>?", with the reason when it can't or didn't.
struct LeaveSheet: View {
    @State var model: LeaveModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Leave \(model.title)?").font(.title2)
            Text("You'll stop receiving its messages. A private channel needs an invitation to rejoin.")
                .fixedSize(horizontal: false, vertical: true)
            if let why = model.error ?? model.warning {
                Text(why).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Leave", role: .destructive) { Task { await model.confirm() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.canLeave)
            }
        }
        .padding(20)
        .frame(width: 400)
        .onChange(of: model.done) { _, done in if done { dismiss() } }
    }
}

/// The open channel's members; Remove where the roles allow it.
struct MembersView: View {
    let title: String
    let powers: ChannelPowers
    @State var model: MembersModel
    @State private var confirming: FfiMember?
    @State private var offering: FfiMember?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Members of \(title)").font(.headline)
            List(powers.rows, id: \.id) { m in
                HStack {
                    VStack(alignment: .leading) {
                        Text(m.id == powers.me ? "\(m.displayName) (you)" : m.displayName)
                        Text("@\(m.handle)").font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if m.role == "owner" { Text("Owner").font(.caption).foregroundStyle(.secondary) }
                    if powers.pending(m) {
                        Text("Owner offered").font(.caption).foregroundStyle(.secondary)
                        if powers.canWithdraw(m) {
                            Button("Withdraw") { Task { await model.withdraw(m) } }
                                .disabled(model.busy != nil)
                        }
                    } else if powers.canOffer(m) {
                        Button("Make Owner…") { offering = m }
                            .disabled(model.busy != nil)
                    }
                    if powers.canRemove(m) {
                        Button("Remove") { confirming = m }
                            .disabled(model.busy != nil)
                    }
                }
            }
            .frame(minHeight: 160)
            if let error = model.error { Text(error).foregroundStyle(.red) }
        }
        .padding(12)
        .frame(width: 320, height: 320)
        .confirmationDialog(
            "Offer \(offering?.displayName ?? "") ownership of \(title)?",
            isPresented: Binding(get: { offering != nil }, set: { if !$0 { offering = nil } })
        ) {
            Button("Offer Ownership") {
                if let m = offering { Task { await model.offer(m) } }
            }
        } message: {
            Text("They'll be asked when they next open it.")
        }
        .confirmationDialog(
            "Remove \(confirming?.displayName ?? "") from \(title)?",
            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } })
        ) {
            Button("Remove", role: .destructive) {
                if let id = confirming?.id { Task { await model.remove(id) } }
            }
        }
    }
}

/// The recipient's question: Accept or Decline, not dismissable until an answer has failed.
struct OfferAnswerSheet: View {
    let model: OfferAnswerModel
    let later: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Become an owner of \(model.title)?").font(.title2)
            Text("\(model.offerer) offered you ownership of \(model.title). Owners can rename, archive and delete it, and remove members.")
                .fixedSize(horizontal: false, vertical: true)
            if let error = model.error { Text(error).foregroundStyle(.red) }
            HStack {
                if model.canDefer { Button("Ask Me Later", action: later) }
                Spacer()
                Button("Decline") { Task { await model.decline() } }
                    .disabled(model.busy)
                Button("Accept") { Task { await model.accept() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(model.busy)
            }
        }
        .padding(20)
        .frame(width: 420)
        .interactiveDismissDisabled()
    }
}
