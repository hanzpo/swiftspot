// swift-tools-version: 6.2
// A native SwiftUI front end for SwiftSpot, with SwiftSpot's Rust engine
// linked in: `cargo build --release --lib` in the repository root makes
// target/release/libswiftspot.a, which keeps the session, playback and
// library (src/ffi.rs).
import PackageDescription

let rustLibrary = Context.packageDirectory + "/../target/release"

let package = Package(
    name: "SwiftSpot",
    platforms: [.macOS("27.0")],
    targets: [
        .systemLibrary(name: "CSwiftSpot"),
        .executableTarget(
            name: "SwiftSpot",
            dependencies: ["CSwiftSpot"],
            linkerSettings: [
                .unsafeFlags(["-L", rustLibrary]),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("CoreML"),
                .linkedFramework("IOKit"),
                .linkedFramework("MediaPlayer"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("iconv"),
            ]
        ),
    ]
)
