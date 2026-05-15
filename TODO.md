- ambiguous-symbol/duplicate-decl: should not error when `geo()` (function) and `geo` (type) have the same symbol, that's valid gcl (`geo` is an example here)
- fmt: is still moving EOL comments to the line below
- fmt: EOL Comment should not remove the space before `//` (it should ensure only 1 appears) in things like:
  ```gcl
  fn main() {
      var x; // formatting will remove the leading whitespace before the EOL comment
  }
  ```
- lsp: rename is not implemented (or not working?)
- lsp: find implementations is not implemented (or not working?)
- lint: new unused-generic-param rule for generic types that don't use there declared generic params (most likely a mistake/oversight)
