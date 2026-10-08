// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit
import SwiftUI

/// The About window: the app, and what the connected server says about itself. It replaces the
/// system About panel, which is built once per open and so cannot show a link that arrives
/// after a network answer.
struct AboutView: View {
    let model: AboutModel

    private var appVersion: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? ""
        let build = info?["CFBundleVersion"] as? String ?? ""
        return "Version \(short) (\(build))"
    }

    /// The copyright and license line is defined once, in project.yml (NSHumanReadableCopyright).
    private var copyright: String {
        Bundle.main.object(forInfoDictionaryKey: "NSHumanReadableCopyright") as? String ?? ""
    }

    var body: some View {
        VStack(spacing: 8) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable().frame(width: 96, height: 96)
            Text("Brook").font(.title).bold()
            Text(appVersion).font(.callout).foregroundStyle(.secondary)
            Text(copyright).font(.caption).foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Divider().padding(.vertical, 4)
            serverLines
        }
        .padding(20)
        .frame(width: 360)
        // Every appearance refreshes, however the window got shown (the command, a restore),
        // and so does every `open()` while it is already shown. A window that refreshed only
        // from the command showed an earlier open's server when shown another way.
        .task(id: model.opens) { await model.refresh() }
    }

    @ViewBuilder
    private var serverLines: some View {
        switch model.line {
        case .noServer:
            Text(AboutModel.Text.noServer).foregroundStyle(.secondary)
        case .loading:
            Text(AboutModel.Text.loading).foregroundStyle(.secondary)
        case .failed:
            Text(AboutModel.Text.failed).foregroundStyle(.secondary)
        case let .loaded(version, sourceText, sourceURL):
            VStack(spacing: 4) {
                Text("\(AboutModel.Text.versionLabel): \(version)")
                Text("\(AboutModel.Text.sourceLabel):")
                // The system browser opens it; the app never fetches the link itself.
                Link(sourceText, destination: sourceURL)
            }
            .textSelection(.enabled)
        }
    }
}

/// The app menu's "About Brook", in place of the system item.
struct AboutCommand: View {
    let model: AboutModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("About Brook") {
            model.open() // changes `opens`, so an already open window fetches again
            openWindow(id: "about")
        }
    }
}
