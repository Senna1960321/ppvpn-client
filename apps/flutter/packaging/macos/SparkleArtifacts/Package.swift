// swift-tools-version: 5.9
import PackageDescription

// Packaging consumes the binary framework and sign_update, without linking
// Sparkle into a build target or rebuilding the retired push agent.
let package = Package(
    name: "PPVPNSparkle",
    dependencies: [.package(url: "https://github.com/sparkle-project/Sparkle", from: "2.6.0")]
)
