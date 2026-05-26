// Shiki integration — `registerGreycat(highlighter)` loads the bundled
// TextMate grammar into a Shiki highlighter so it can tokenize `.gcl`
// sources. The grammar itself is exposed via `@greycat/shiki/grammar`
// for consumers that want to feed it to a different highlighter
// implementation.
//
// No `@greycat/analyzer` dependency: this package is pure-syntax
// highlighting via TextMate, independent of the wasm analyzer.

import type { HighlighterCore, LanguageRegistration } from "shiki";

import { greycatGrammar } from "./grammar.generated.js";

export { greycatGrammar } from "./grammar.generated.js";

export const GREYCAT_LANG_ID = "greycat";

/** Load the GreyCat grammar into `highlighter` so subsequent calls to
 *  `codeToHtml({ code, lang: "greycat" })` tokenize it correctly. */
export async function registerGreycat(highlighter: HighlighterCore): Promise<void> {
  await highlighter.loadLanguage(greycatGrammar as unknown as LanguageRegistration);
}
