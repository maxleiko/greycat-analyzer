/// <reference path="./pkg/greycat_analyzer_wasm.d.ts" />

// Declare that `monaco` exists in the global scope
declare const monaco: typeof import('monaco-editor');

// Well-known elements IDs in the DOM (see index.html)
declare const tree: HTMLElement;
declare const editor: HTMLElement;
declare const lineNumbers: HTMLElement;
