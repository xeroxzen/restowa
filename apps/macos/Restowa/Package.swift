// swift-tools-version: 6.2
import PackageDescription

let strict: [SwiftSetting] = [.treatAllWarnings(as: .error)]

let package = Package(
    name: "Restowa",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "Restowa", targets: ["Restowa"])
    ],
    targets: [
        .target(name: "RestowaKit", swiftSettings: strict),
        .executableTarget(
            name: "Restowa",
            dependencies: ["RestowaKit"],
            swiftSettings: strict
        ),
        .testTarget(
            name: "RestowaKitTests",
            dependencies: ["RestowaKit"],
            swiftSettings: strict
        ),
    ]
)
