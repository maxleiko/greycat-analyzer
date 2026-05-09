// JS-side stubs for the libc symbols the wasm imports under the
// `env` namespace. Tree-sitter's C scanner (compiled to wasm via
// clang) references a couple of `<wctype.h>` predicates that the
// host environment is expected to provide; wasm-bindgen forwards
// the `env` import object through to the wasm linker untouched, so
// we satisfy them in JS.
//
// Vite resolves `import * as __wbg_star0 from 'env'` to this file
// via the alias in `vite.config.ts`. Each stub takes the same
// argument shape as its libc counterpart (`int (int)`) and returns
// 0 for "no" / non-zero for "yes".

/// `iswalpha(c)` — true when `c` is a Unicode letter. Defers to
/// `\p{L}` in a regex; tree-sitter only calls it during ident
/// scanning so a few-thousand calls per parse is fine.
export function iswalpha(c: number): number {
  // ASCII fast path — the overwhelmingly common case for code.
  if (c < 0x80) {
    return (c >= 0x41 && c <= 0x5a) || (c >= 0x61 && c <= 0x7a) ? 1 : 0;
  }
  try {
    return /\p{L}/u.test(String.fromCodePoint(c)) ? 1 : 0;
  } catch {
    // `String.fromCodePoint` throws on invalid scalar values
    // (surrogates, > 0x10FFFF). Treat as not-a-letter.
    return 0;
  }
}
