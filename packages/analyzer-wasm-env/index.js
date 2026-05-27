// JS-side stubs for the libc symbols the analyzer's wasm imports
// under the `env` namespace. Tree-sitter's C scanner (compiled to
// wasm via clang) references `<wctype.h>` predicates that the host
// environment is expected to provide; wasm-bindgen forwards the
// `env` import object through to the wasm linker untouched, so we
// satisfy them in JS.
//
// The wasm-pack-generated JS in `@greycat/analyzer-wasm` imports
// this module by name — that rewrite happens in `scripts/build-
// wasm.sh` after wasm-pack runs (wasm-bindgen's JS would otherwise
// say `from "env"`, which isn't a real package).

// `iswalpha(c)` — true when `c` is a Unicode letter. Defers to
// `\p{L}` in a regex; tree-sitter only calls it during ident
// scanning so a few-thousand calls per parse is fine.
export function iswalpha(c) {
  if (c < 0x80) {
    return (c >= 0x41 && c <= 0x5a) || (c >= 0x61 && c <= 0x7a) ? 1 : 0;
  }
  try {
    return /\p{L}/u.test(String.fromCodePoint(c)) ? 1 : 0;
  } catch {
    return 0;
  }
}
