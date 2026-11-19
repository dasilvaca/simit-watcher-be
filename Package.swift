// swift-tools-version: 6.2
// SappaCore: the Rust core (static library in an xcframework) plus its generated Swift API.
// Build the binary first with `scripts/build-ios.sh`; it writes dist/ios/SappaCore.xcframework
// and swift/Sources/SappaCore/SappaCore.swift.
import PackageDescription

let package = Package(
    name: "SappaCore",
    platforms: [.iOS(.v26)],
    products: [
        .library(name: "SappaCore", targets: ["SappaCore"]),
    ],
    targets: [
        // C module `sappa_coreFFI` (header + module map inside the xcframework).
        .binaryTarget(name: "sappa_coreFFI", path: "dist/ios/SappaCore.xcframework"),
        .target(
            name: "SappaCore",
            dependencies: ["sappa_coreFFI"],
            path: "swift/Sources/SappaCore",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
