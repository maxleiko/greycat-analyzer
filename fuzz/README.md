# Fuzz targets (P10.2)

Three [`cargo-fuzz`](https://rust-fuzz.github.io/book/cargo-fuzz.html)
targets exercising the parser / HIR / formatter boundaries on
arbitrary UTF-8 input. They share the workspace's crate dependencies
but live outside the workspace (see `Cargo.toml::workspace.exclude`)
so `libfuzzer-sys` doesn't pollute regular `cargo build --workspace`
runs.

## Running

```sh
# install once
cargo install cargo-fuzz   # nightly toolchain required

# from the workspace root:
cd fuzz
cargo +nightly fuzz run parser              # arbitrary UTF-8 → parse
cargo +nightly fuzz run hir_lower           # arbitrary UTF-8 → parse → HIR lower
cargo +nightly fuzz run format_round_trip   # parse → fmt → parse round-trip
```

Each target runs forever; press `Ctrl-C` to stop. Crashes land in
`fuzz/artifacts/<target>/`.

## Targets

- `parser` — `greycat_analyzer_syntax::parse(s)` shouldn't panic
  on any UTF-8 input. The grammar's external scanner is the most
  likely panic source, so this is the cheapest insurance available.
- `hir_lower` — `parse → lower_module` together. Catches HIR
  panics on legal-but-unusual CST shapes (deeply nested generics,
  zero-named-children blocks, etc.).
- `format_round_trip` — `parse → format_tree → parse` on
  initially-error-free input. Catches formatter outputs that
  re-parse with errors. Doesn't assert byte-for-byte parity (that's
  P9.2's job).

## Adding a target

1. Create `fuzz_targets/<name>.rs` with a `fuzz_target!` macro.
2. Add a `[[bin]]` entry to `Cargo.toml` pointing at it.
3. Document it here.
