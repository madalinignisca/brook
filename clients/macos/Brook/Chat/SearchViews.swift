import BrookCore
import SwiftUI

/// Search results, in place of the channel list while a search is showing.
struct SearchResultsView: View {
    let model: SearchModel
    /// A channel's title, for a hit's channel.
    let title: (String) -> String
    let onOpen: (String) -> Void
    @Environment(\.showUsernames) private var showUsernames

    var body: some View {
        switch model.state {
        case .idle:
            EmptyView()
        case .searching:
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
        case .none:
            Text("No messages found.").foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed:
            Text(SearchModel.offlineText).foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
        case .tooLong:
            Text("Search terms can be up to \(SearchModel.queryLimit) characters.").foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case let .results(hits):
            List(hits) { hit in
                Button { onOpen(hit.channelId) } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("\(title(hit.channelId)) · \(hit.author(showUsernames: showUsernames))").font(.caption).foregroundStyle(.secondary)
                        Text(hit.excerpt).lineLimit(2)
                    }
                }
                .buttonStyle(.plain)
            }
            .safeAreaInset(edge: .bottom) {
                if model.capped {
                    Text("Showing the newest \(SearchModel.resultCap) matches.").font(.caption)
                        .foregroundStyle(.secondary).padding(6)
                }
            }
        }
    }
}

/// The search field (in the sidebar) and its results, when this client can search.
struct SearchPresentation: ViewModifier {
    let model: SearchModel?
    let title: (String) -> String
    let onOpen: (String) -> Void

    @ViewBuilder func body(content: Content) -> some View {
        if let model {
            content
                .overlay {
                    if model.isShowing {
                        SearchResultsView(model: model, title: title, onOpen: onOpen).background(.background)
                    }
                }
                .searchable(
                    text: Binding(get: { model.query }, set: { $0.isEmpty ? model.clear() : (model.query = $0) }),
                    placement: .sidebar, prompt: "Search messages")
                .onSubmit(of: .search) { Task { await model.submit() } }
        } else {
            content
        }
    }
}
