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
        .library(name: "BrookMedia", targets: ["BrookMedia"]),
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
        // libwebrtc M153 (github.com/stasel/WebRTC, built by public CI from chromium sources);
        // checksum = upstream Package.swift, re-verified before adoption. H.264 is VideoToolbox.
        .binaryTarget(
            name: "WebRTC",
            url: "https://github.com/stasel/WebRTC/releases/download/153.0.0/WebRTC-M153.xcframework.zip",
            checksum: "3e3a8946f27510133e3feed04d05fa23505bbe366e977620503bfc7986c2b78f"
        ),
        // `RTCAudioDevice` for macOS, whose WebRTC slice lacks the header (see the header).
        .target(name: "WebRTCAudioDevice", dependencies: ["WebRTC"], exclude: ["LICENSE.webrtc"]),
        // The call media engine (core's MediaEngine over libwebrtc).
        .target(name: "BrookMedia", dependencies: ["BrookCore", "WebRTC", "WebRTCAudioDevice"]),
        .testTarget(name: "BrookMediaTests", dependencies: ["BrookMedia"]),
    ]
)
