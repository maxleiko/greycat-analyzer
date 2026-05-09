// Monarch tokenizer + language configuration for `.gcl`.
// Yoinked from the TS reference playground (packages/playground/src/xp-editor/greycat-language)
// and reduced to the syntax-highlighting bits.
//
// Plus a `DocumentFormattingEditProvider` that runs the wasm formatter
// from the worker. Monaco wires the standard `editor.action.formatDocument`
// command (Shift+Alt+F / right-click → Format Document) through this
// provider. Replaces the old "Format" side-panel: the formatter belongs
// inside the editor itself, not as a separate read-only diff.

import * as monaco from "monaco-editor";
import { getAnalyzer } from "./analyzer-client.ts";

const language: monaco.languages.IMonarchLanguage = {
  defaultToken: "",
  tokenPostfix: ".gcl",

  keywords: [
    "abstract",
    "as",
    "at",
    "break",
    "breakpoint",
    "catch",
    "continue",
    "do",
    "else",
    "enum",
    "extends",
    "false",
    "fn",
    "for",
    "if",
    "in",
    "is",
    "native",
    "private",
    "return",
    "static",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "while",
  ],

  typeKeywords: [
    "any",
    "bool",
    "char",
    "duration",
    "field",
    "float",
    "function",
    "geo",
    "int",
    "node",
    "nodeGeo",
    "nodeIndex",
    "nodeList",
    "nodeTime",
    "str",
    "t2",
    "t3",
    "t4",
    "tf2",
    "tf3",
    "tf4",
    "time",
    "type",
    "var",
  ],

  operators: [
    "=",
    ">",
    "<",
    "!",
    "~",
    "?",
    ":",
    "==",
    "<=",
    ">=",
    "!=",
    "&&",
    "||",
    "++",
    "--",
    "+",
    "-",
    "*",
    "/",
    "&",
    "|",
    "^",
    "%",
    "->",
    ".",
    "::",
  ],

  symbols: /[=><!~?:&|+\-*/^%]+/,
  escapes: /\\(?:[abfnrtv\\"']|x[0-9A-Fa-f]{1,4}|u[0-9A-Fa-f]{4}|U[0-9A-Fa-f]{8})/,

  tokenizer: {
    root: [
      [
        /[a-z_$][\w$]*/,
        {
          cases: {
            "@typeKeywords": "keyword.type",
            "@keywords": "keyword",
            "@default": "identifier",
          },
        },
      ],
      [/[A-Z][\w$]*/, "type.identifier"],

      { include: "@whitespace" },

      [/[{}()[\]]/, "@brackets"],
      [/[<>](?!@symbols)/, "@brackets"],
      [/@symbols/, { cases: { "@operators": "operator", "@default": "" } }],

      [/@\s*[a-zA-Z_$][\w$]*/, "annotation"],

      [/\d*\.\d+([eE][-+]?\d+)?/, "number.float"],
      [/0[xX][0-9a-fA-F]+/, "number.hex"],
      [/\d+/, "number"],

      [/[;,.]/, "delimiter"],

      [/"([^"\\]|\\.)*$/, "string.invalid"],
      [/"/, { token: "string.quote", bracket: "@open", next: "@string" }],

      [/'[^\\']'/, "string"],
      [/(')(@escapes)(')/, ["string", "string.escape", "string"]],
      [/'/, "string.invalid"],
    ],

    comment: [
      [/[^/*]+/, "comment"],
      [/\/\*/, "comment", "@push"],
      [/\*\//, "comment", "@pop"],
      [/[/*]/, "comment"],
    ],

    string: [
      [/[^\\"]+/, "string"],
      [/@escapes/, "string.escape"],
      [/\\./, "string.escape.invalid"],
      [/"/, { token: "string.quote", bracket: "@close", next: "@pop" }],
    ],

    whitespace: [
      [/[ \t\r\n]+/, "white"],
      [/\/\*/, "comment", "@comment"],
      [/\/\/.*$/, "comment"],
    ],
  },
};

const configuration: monaco.languages.LanguageConfiguration = {
  comments: { blockComment: ["/*", "*/"], lineComment: "//" },
  brackets: [
    ["{", "}"],
    ["[", "]"],
    ["(", ")"],
  ],
  autoClosingPairs: [
    { open: "{", close: "}" },
    { open: "[", close: "]" },
    { open: "(", close: ")" },
    { open: '"', close: '"', notIn: ["string"] },
    { open: "'", close: "'", notIn: ["string", "comment"] },
  ],
  surroundingPairs: [
    { open: "{", close: "}" },
    { open: "[", close: "]" },
    { open: "(", close: ")" },
    { open: '"', close: '"' },
    { open: "'", close: "'" },
  ],
};

export function registerGcl() {
  if (monaco.languages.getLanguages().some((l) => l.id === "gcl")) return;
  monaco.languages.register({
    id: "gcl",
    aliases: ["greycat", "GreyCat"],
    extensions: [".gcl"],
  });
  monaco.languages.setMonarchTokensProvider("gcl", language);
  monaco.languages.setLanguageConfiguration("gcl", configuration);

  // Format-document provider. Returns a single full-document replace
  // edit when the formatter's output differs from the input, or no
  // edits when the doc is already clean (matches Monaco's "no changes
  // needed" notification). The diff is the whole buffer because the
  // wasm formatter is whole-file; a per-region edit would need
  // greycat_analyzer_fmt to support partial formatting.
  monaco.languages.registerDocumentFormattingEditProvider("gcl", {
    async provideDocumentFormattingEdits(model) {
      const text = model.getValue();
      const formatted = await getAnalyzer().format(text);
      if (formatted === text) {
        return [];
      }
      return [
        {
          range: model.getFullModelRange(),
          text: formatted,
        },
      ];
    },
  });
}
