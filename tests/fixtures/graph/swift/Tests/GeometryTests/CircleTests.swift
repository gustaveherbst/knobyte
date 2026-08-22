import XCTest
@testable import Geometry

final class CircleTests: XCTestCase {
    func testMove() {
        let c: Circle = makeCircle()
        XCTAssertTrue(c.move(to: Point(x: 1, y: 1)))
        var h = History()
        h.push(Point.origin)
    }
}
