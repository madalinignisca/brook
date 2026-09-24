import SwiftUI

@main
struct BrookApp: App {
    var body: some Scene {
        Window("Brook", id: "main") {
            Text("Brook")
                .frame(minWidth: 380, minHeight: 480)
        }
        .defaultSize(width: 420, height: 560)
    }
}
