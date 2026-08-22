// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Shapes",
    products: [
        .executable(name: "shapes", targets: ["App"]),
        .library(name: "Geometry", targets: ["Geometry"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-argument-parser.git", from: "1.3.0"),
    ],
    targets: [
        .target(name: "Geometry"),
        .executableTarget(name: "App", dependencies: [
            "Geometry",
            .product(name: "ArgumentParser", package: "swift-argument-parser"),
        ]),
        .testTarget(name: "GeometryTests", dependencies: ["Geometry"]),
    ]
)
