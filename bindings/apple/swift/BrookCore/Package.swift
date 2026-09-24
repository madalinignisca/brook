// swift-tools-version: 6.2
// BrookCore: Swift bindings of the shared Rust client core (brook-core via brook-ffi).
// `BrookCoreFFI.xcframework` and `Sources/BrookCoreGenerated/` are build outputs of
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
        // UniFFI's generated Swift. Swift 5 language mode: its async foreign-trait glue does not
        // compile under Swift 6's region-isolation checks (verified by spike, UniFFI 0.32.2).
        // Everything hand-written stays in Swift 6 in `BrookCore`, which re-exports this.
        .target(
            name: "BrookCoreGenerated",
            dependencies: ["BrookCoreFFI"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .target(name: "BrookCore", dependencies: ["BrookCoreGenerated"]),
        .testTarget(name: "BrookCoreTests", dependencies: ["BrookCore"]),
    ]
)
