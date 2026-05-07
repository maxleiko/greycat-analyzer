// Playground entry point. Pulls in WebAwesome (single bundled
// stylesheet + auto-loader), our app stylesheet, then registers each
// Lit component.

import "@awesome.me/webawesome/dist/styles/webawesome.css";
import "@awesome.me/webawesome/dist/webawesome.loader.js";

import "./style.css";
import "./components/gc-playground.ts";
import "./components/gc-editor.ts";
import "./components/gc-cst-panel.ts";
import "./components/gc-hir-panel.ts";
import "./components/gc-tokens-panel.ts";
import "./components/gc-types-panel.ts";
import "./components/gc-diagnostics-panel.ts";
import "./components/gc-format-panel.ts";
