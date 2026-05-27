// Monarch tokenizer + language configuration for GreyCat.
//
// The analyzer emits semantic tokens (string / number / comment /
// keyword / function / type / enum / enumMember / variable /
// parameter) which Monaco layers over the base tokenization for
// finer-grained coloring. But default themes (`vs`, `vs-dark`) only
// color the base tokens, so without a Monarch grammar the editor
// renders everything as plain text.
//
// This grammar mirrors the keyword / operator / literal shape of
// `editors/code/grammar/Greycat.tmLanguage.json` — close enough that
// the playground matches the VSCode look without dragging a full TM
// engine in.

import type * as MonacoNs from "monaco-editor";

const KEYWORDS = [
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
  "fn",
  "for",
  "if",
  "in",
  "is",
  "limit",
  "native",
  "private",
  "return",
  "sampling",
  "skip",
  "static",
  "task",
  "throw",
  "try",
  "type",
  "use",
  "var",
  "while",
  "without",
];

const CONSTANTS = ["true", "false", "null", "NaN", "Infinity", "this"];

const TYPE_KEYWORDS = [
  "any",
  "Array",
  "bool",
  "char",
  "duration",
  "f8",
  "f16",
  "f32",
  "f64",
  "field",
  "float",
  "function",
  "geo",
  "i8",
  "i16",
  "i32",
  "i64",
  "int",
  "Map",
  "node",
  "nodeGeo",
  "nodeIndex",
  "nodeList",
  "nodeTime",
  "Set",
  "String",
  "time",
  "tuple",
  "u8",
  "u16",
  "u32",
  "u64",
];

export const MONARCH_LANGUAGE: MonacoNs.languages.IMonarchLanguage = {
  defaultToken: "",
  tokenPostfix: ".gcl",

  keywords: KEYWORDS,
  constants: CONSTANTS,
  typeKeywords: TYPE_KEYWORDS,

  operators: [
    "=",
    "?=",
    "==",
    "!=",
    "<=",
    ">=",
    "<",
    ">",
    "&&",
    "||",
    "!",
    "??",
    "!!",
    "?",
    "+",
    "-",
    "*",
    "/",
    "%",
    "->",
    "::",
    "..",
  ],

  symbols: /[=><!~?:&|+\-*/^%]+/,

  escapes: /\\(?:[abfnrtv\\"']|x[0-9A-Fa-f]{1,4}|u[0-9A-Fa-f]{4})/,

  tokenizer: {
    root: [
      // doc comments first (more specific than `//`)
      [/\/\/\/.*$/, "comment.doc"],
      [/\/\/.*$/, "comment"],
      [/\/\*/, "comment", "@comment"],

      // strings
      [/"([^"\\]|\\.)*$/, "string.invalid"],
      [/"/, { token: "string.quote", bracket: "@open", next: "@dstring" }],
      [/'([^'\\]|\\.)*$/, "string.invalid"],
      [/'/, { token: "string.quote", bracket: "@open", next: "@sstring" }],

      // annotations (@doc, @library, @include, …)
      [/@[a-zA-Z_]\w*/, "annotation"],

      // numbers with optional suffix (42, 42i32, 1.5e3_f, 30s, …)
      [/\d[\d_]*\.\d[\d_]*(?:[eE][+-]?\d[\d_]*)?(?:[a-zA-Z_]\w*)?/, "number.float"],
      [/\d[\d_]*[eE][+-]?\d[\d_]*(?:[a-zA-Z_]\w*)?/, "number.float"],
      [/\d[\d_]*(?:[a-zA-Z_]\w*)?/, "number"],

      // identifiers and keywords — order matters: constants/types/keywords first
      [
        /[a-zA-Z_]\w*/,
        {
          cases: {
            "@constants": "keyword",
            "@typeKeywords": "type",
            "@keywords": "keyword",
            "@default": "identifier",
          },
        },
      ],

      // delimiters / operators
      [/[{}()[\]]/, "@brackets"],
      [
        /@symbols/,
        {
          cases: {
            "@operators": "operator",
            "@default": "",
          },
        },
      ],
      [/[;,.]/, "delimiter"],

      // whitespace
      [/[ \t\r\n]+/, ""],
    ],

    comment: [
      [/[^/*]+/, "comment"],
      [/\*\//, "comment", "@pop"],
      [/[/*]/, "comment"],
    ],

    dstring: [
      [/[^\\"]+/, "string"],
      [/@escapes/, "string.escape"],
      [/\\./, "string.escape.invalid"],
      [/"/, { token: "string.quote", bracket: "@close", next: "@pop" }],
    ],

    sstring: [
      [/[^\\']+/, "string"],
      [/@escapes/, "string.escape"],
      [/\\./, "string.escape.invalid"],
      [/'/, { token: "string.quote", bracket: "@close", next: "@pop" }],
    ],
  },
};

export const LANGUAGE_CONFIGURATION: MonacoNs.languages.LanguageConfiguration = {
  comments: {
    lineComment: "//",
    blockComment: ["/*", "*/"],
  },
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
    { open: "'", close: "'", notIn: ["string"] },
  ],
  surroundingPairs: [
    { open: "{", close: "}" },
    { open: "[", close: "]" },
    { open: "(", close: ")" },
    { open: '"', close: '"' },
    { open: "'", close: "'" },
  ],
};
