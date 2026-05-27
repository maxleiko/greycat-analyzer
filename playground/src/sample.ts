// Default source loaded when the playground first opens. Standalone
// (no `@library` pragma) so the Monaco demo doesn't need network
// access to a registry — every type the analyzer needs is defined
// in-file, and the resolver is never invoked.

export const SAMPLE_SOURCE = `enum Color {
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

fn demo(): int {
    var p = Point { x: 3, y: 4 };
    return add(p.distance_sq(), 0);
}
`;
