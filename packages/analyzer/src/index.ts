// Public surface for `@greycat/analyzer`.
//
// The TOP-LEVEL `Project` is the reactive JS-side wrapper from
// `./project.ts`. It owns the `@library` pragma → registry resolve →
// rebuild lifecycle and forwards every analyzer call to the wasm
// `Project` underneath.
//
// The wasm-emitted ADTs (`Diagnostic`, `Hover`, `CompletionList`, …)
// are re-exported as the wire shapes consumers see when calling the
// reactive `Project`'s methods.

export { Project } from "./project.js";
export type { ProjectCreateOptions } from "./project.js";

export {
  InMemoryContext,
  MonacoContext,
  mergeContexts,
  type Context,
  type MonacoEditorNamespace,
} from "./context.js";

export {
  IndexedDbLibraryCache,
  MemoryLibraryCache,
  NoopLibraryCache,
  RegistryLibraryResolver,
  registryUrlFor,
  type LibraryCache,
  type LibraryResolver,
  type RegistryLibraryResolverOptions,
} from "./library-resolver.js";

// Wasm-emitted ADTs — consumed by `@greycat/monaco` and any other
// downstream that walks analyzer results.
export {
  CodeActionKind,
  CompletionItemKind,
  InsertTextFormat,
  IdeSeverity,
  IdeTag,
  InlayHintKind,
  SymbolKind,
  type CodeAction,
  type CompletionItem,
  type CompletionItemLabelDetails,
  type CompletionList,
  type DocumentHighlight,
  type DocumentSymbol,
  type FoldingRange,
  type Hover,
  type IdeDiagnostic as Diagnostic,
  type IdePosition,
  type IdeRange,
  type InlayHint,
  type Location,
  type ParameterInformation,
  type RenameTarget,
  type SemanticTokens,
  type SignatureHelp,
  type SignatureInformation,
  type TextEdit,
  type UriEdits,
  type WorkspaceSymbol,
} from "../wasm/greycat_analyzer_wasm.js";
