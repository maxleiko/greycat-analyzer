// Monaco `RenameProvider` backed by `Project::resolveRenameTarget` +
// `renameTargetSites`. The IDE shape is two-step (classify, then
// enumerate sites); Monaco's API is a single `provideRenameEdits`
// call that returns the edits to apply, plus an optional
// `resolveRenameLocation` for the inline rename popover's initial
// range.

import type * as MonacoNs from "monaco-editor";
import type { Project } from "@greycat/analyzer";

export function registerRename(
  monaco: typeof MonacoNs,
  project: Project,
  languageId: string,
): MonacoNs.IDisposable {
  return monaco.languages.registerRenameProvider(languageId, {
    resolveRenameLocation(model, position) {
      const target = project.resolveRenameTarget(
        model.uri.toString(),
        position.lineNumber - 1,
        position.column - 1,
      );
      if (!target) {
        return {
          text: "",
          range: {
            startLineNumber: position.lineNumber,
            startColumn: position.column,
            endLineNumber: position.lineNumber,
            endColumn: position.column,
          },
          rejectReason: "no rename target at cursor",
        };
      }
      const sites = project.renameTargetSites(target);
      // The site whose URI + range straddles the cursor is the
      // "name" range Monaco shows in the inline popover.
      const here = sites.find((loc) => {
        if (loc.uri !== model.uri.toString()) {
          return false;
        }
        const startLine = loc.range.start.line + 1;
        const endLine = loc.range.end.line + 1;
        const startCol = loc.range.start.character + 1;
        const endCol = loc.range.end.character + 1;
        if (position.lineNumber < startLine || position.lineNumber > endLine) {
          return false;
        }
        if (position.lineNumber === startLine && position.column < startCol) {
          return false;
        }
        if (position.lineNumber === endLine && position.column > endCol) {
          return false;
        }
        return true;
      });
      const range = here ?? sites[0];
      if (!range) {
        return {
          text: "",
          range: {
            startLineNumber: position.lineNumber,
            startColumn: position.column,
            endLineNumber: position.lineNumber,
            endColumn: position.column,
          },
          rejectReason: "rename target has no sites",
        };
      }
      const text = model.getValueInRange({
        startLineNumber: range.range.start.line + 1,
        startColumn: range.range.start.character + 1,
        endLineNumber: range.range.end.line + 1,
        endColumn: range.range.end.character + 1,
      });
      return {
        text,
        range: {
          startLineNumber: range.range.start.line + 1,
          startColumn: range.range.start.character + 1,
          endLineNumber: range.range.end.line + 1,
          endColumn: range.range.end.character + 1,
        },
      };
    },
    provideRenameEdits(model, position, newName) {
      const target = project.resolveRenameTarget(
        model.uri.toString(),
        position.lineNumber - 1,
        position.column - 1,
      );
      if (!target) {
        return { edits: [], rejectReason: "no rename target at cursor" };
      }
      const sites = project.renameTargetSites(target);
      return {
        edits: sites.map((loc) => ({
          resource: monaco.Uri.parse(loc.uri),
          textEdit: {
            range: {
              startLineNumber: loc.range.start.line + 1,
              startColumn: loc.range.start.character + 1,
              endLineNumber: loc.range.end.line + 1,
              endColumn: loc.range.end.character + 1,
            },
            text: newName,
          },
          versionId: undefined,
        })),
      };
    },
  });
}
