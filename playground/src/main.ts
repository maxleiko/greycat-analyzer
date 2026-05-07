// Playground entry point. Wires up WebAwesome, registers Lit components,
// and mounts the root `<gc-playground>` element from index.html.

import "@awesome.me/webawesome/dist/components/split-panel/split-panel.js";
import "@awesome.me/webawesome/dist/components/tab-group/tab-group.js";
import "@awesome.me/webawesome/dist/components/tab/tab.js";
import "@awesome.me/webawesome/dist/components/tab-panel/tab-panel.js";
import "@awesome.me/webawesome/dist/components/details/details.js";
import "@awesome.me/webawesome/dist/components/badge/badge.js";
import "@awesome.me/webawesome/dist/components/icon/icon.js";

import "./style.css";
import "./components/gc-playground.ts";
import "./components/gc-editor.ts";
import "./components/gc-cst-panel.ts";
import "./components/gc-hir-panel.ts";
import "./components/gc-tokens-panel.ts";
import "./components/gc-types-panel.ts";
import "./components/gc-diagnostics-panel.ts";
import "./components/gc-format-panel.ts";
