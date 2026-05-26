// Public re-export of the bundled TextMate grammar. Lives in its own
// entry so consumers can `import { greycatGrammar } from "@greycat/shiki/grammar"`
// without pulling the `register` helper (Shiki's own
// `LanguageRegistration` type isn't needed at type-level for this
// path).

export { greycatGrammar } from "./grammar.generated.js";
