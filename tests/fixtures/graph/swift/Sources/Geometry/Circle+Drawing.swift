extension Circle: Drawable {
    public func draw() {
        let text = describe()
        render(text)
    }

    func render(_ text: String) {
        print(text)
    }
}
