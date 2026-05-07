// Default source loaded when the playground first opens. A small but
// real GreyCat snippet that exercises every analyzer stage:
// - module pragma (@library)
// - type declaration with attrs and a method
// - enum
// - function with parameters, locals, and a binary expression.

export const SAMPLE_SOURCE = `@library("std", "1.0");

enum Color {
    Red,
    Green,
    Blue,
}

type Point {
    x: int;
    y: int;

    fn distance_sq(): int {
        return this.x * this.x + this.y * this.y;
    }
}

fn add(a: int, b: int): int {
    return a + b;
}

fn unused_demo(): int {
    var unused: int = 42;
    return add(1, 2);
}
`;
