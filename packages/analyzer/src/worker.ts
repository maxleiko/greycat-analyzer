// Web Worker entry — hosts a JS-side `Project` (the reactive wrapper,
// not the raw wasm handle) on a worker thread and exposes its methods
// via Comlink.
//
// Consumer wiring:
//
//     import { wrap, type Remote } from "comlink";
//     import type { Project as ProjectClass } from "@greycat/analyzer";
//
//     const worker = new Worker(
//       new URL("@greycat/analyzer/worker", import.meta.url),
//       { type: "module" },
//     );
//     const RemoteProject = wrap<typeof ProjectClass>(worker);
//     const project = await RemoteProject.create({ ... });
//     const diags = await project.diagnostics("file:///main.gcl");
//
// All `Project` methods are sync in the main-thread surface; Comlink
// Promise-wraps them automatically across the worker boundary. The
// `Project.create` static returns a `Promise<Project>` either way
// because library resolution is intrinsically async.

import { expose } from "comlink";
import { Project } from "./project.js";

expose(Project);
