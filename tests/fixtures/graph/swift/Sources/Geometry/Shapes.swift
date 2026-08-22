import Foundation

/// Anything with an area.
public protocol Shape: AnyObject {
    var area: Double { get }
    func move(to point: Point) -> Bool
    init(name: String)
}

public protocol Drawable {
    func draw()
}

public struct Point: Equatable {
    public var x: Double
    public var y: Double
    public static let origin = Point(x: 0, y: 0)
}

public typealias Coordinates = Point

/// Base class of every concrete shape.
open class BaseShape {
    public let name: String

    public init(name: String) {
        self.name = name
    }

    open func describe() -> String {
        return name
    }

    deinit {}
}

/// A circle.
public final class Circle: BaseShape, Shape {
    public static var count = 0
    private var center: Point = Point(x: 0, y: 0)
    let radius: Double
    var history: History = History()

    public var area: Double {
        return Circle.square(radius) * 3.14
    }

    public required init(name: String) {
        self.radius = 1
        super.init(name: name)
    }

    public init(radius: Double) {
        self.radius = radius
        super.init(name: "circle")
        Circle.count += 1
    }

    public override func describe() -> String {
        let base = super.describe()
        record()
        return base
    }

    @discardableResult
    public func move(to point: Point) -> Bool {
        center = point
        history.push(point)
        return true
    }

    public func move(by dx: Double, _ dy: Double) -> Bool {
        let target = Point(x: center.x + dx, y: center.y + dy)
        return move(to: target)
    }

    static func square(_ v: Double) -> Double { v * v }

    fileprivate func record() {
        self.history.push(center)
    }

    subscript(index: Int) -> Point {
        return center
    }

    /// A nested type.
    enum Style: String {
        case filled = "f", outlined
        case dashed(width: Int)
    }
}

struct History {
    private(set) var points: [Point] = []

    mutating func push(_ p: Point) {
        points.append(p)
    }
}

actor Counter {
    private var value = 0

    func increment() -> Int {
        value += 1
        return value
    }
}

struct Box<Element> {
    var items: [Element] = []
}

let defaultRadius = 1.0
var shapeCount = 0

public func makeCircle() -> Circle {
    let c = Circle(radius: defaultRadius)
    c.move(to: Point.origin)
    return c
}

/// A protocol first in a class inheritance clause is a conformance, not a superclass.
final class Marker: Drawable {
    func draw() {
        Logger.shared.log(.info, "marker")
        let logger = Logger()
        _ = logger.fail(code: 2)
    }
}

enum Level { case info, error(code: Int) }

final class Logger {
    static let shared = Logger()

    func log(_ level: Level, _ message: String) {}

    func fail(code: Int) -> Level {
        return Level.error(code: code)
    }
}
