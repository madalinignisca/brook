// swift-tools-version: 6.2
// BrookCore: Swift bindings of the shared Rust client core (brook-core via brook-ffi).
// `BrookCoreFFI.xcframework` and `Sources/BrookCore/Generated/` are build outputs of
// `../../build-xcframework.sh` — run it before building this package.
import PackageDescription

let package = Package(
    name: "BrookCore",
    // Keep in step with MACOSX_DEPLOYMENT_TARGET in build-xcframework.sh. iOS joins later.
    platforms: [.macOS(.v26)],
    products: [
        .library(name: "BrookCore", targets: ["BrookCore"]),
    ],
    targets: [
        .binaryTarget(name: "BrookCoreFFI", path: "BrookCoreFFI.xcframework"),
        .target(name: "BrookCore", dependencies: ["BrookCoreFFI"]),
        .testTarget(name: "BrookCoreTests", dependencies: ["BrookCore"]),
    ]
)
