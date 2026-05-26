//! Thin converter from the IDE-shape `analysis::ide::completion::*`
//! ADTs to `lsp_types::CompletionList` + `CompletionItem`. The dispatcher,
//! scope walks, member discovery, lib-version placeholder + resolution
//! all live in the analysis crate so the CLI and wasm bridge share the
//! same source.

use greycat_analyzer_analysis::ide::completion::{
    CompletionItem as IdeCompletionItem, CompletionItemKind as IdeCompletionItemKind,
    CompletionItemLabelDetails as IdeCompletionItemLabelDetails,
    CompletionList as IdeCompletionList, InsertTextFormat as IdeInsertTextFormat,
    LibVersionPayload, completion_with_project as completion_inner,
    extract_lib_version_placeholder as extract_lib_version_placeholder_inner,
    resolve_library_version_completion as resolve_library_version_completion_inner,
};
use greycat_analyzer_analysis::ide::types::{
    Position as IdePosition, Range as IdeRange, TextEdit as IdeTextEdit,
};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::{SourceEncoding, registry::RegistryFetcher};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList,
    CompletionTextEdit, Documentation, InsertTextFormat, MarkupContent, MarkupKind, Position,
    Range, TextEdit, Uri,
};

#[allow(clippy::too_many_arguments)]
pub fn completion_with_project(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    uri: &Uri,
    project: &ProjectAnalysis,
    project_root: Option<&std::path::Path>,
    encoding: SourceEncoding,
) -> Option<CompletionList> {
    completion_inner(text, root, pos, uri, project, project_root, encoding).map(list_to_lsp)
}

/// LSP-side projection of the IDE placeholder extractor. Reads the
/// LSP-shape `CompletionList` by walking back through its single item's
/// `data` field — same JSON shape on both sides.
pub fn extract_lib_version_placeholder(list: &CompletionList) -> Option<LibVersionPayload> {
    // Round-trip through the IDE shape: the JSON `data` payload is the
    // same shape regardless of the carrier struct, so we reuse the
    // analysis-side extractor.
    let ide_list = lsp_list_to_ide(list);
    extract_lib_version_placeholder_inner(&ide_list)
}

pub fn resolve_library_version_completion(
    payload: &LibVersionPayload,
    fetcher: &dyn RegistryFetcher,
) -> CompletionList {
    list_to_lsp(resolve_library_version_completion_inner(payload, fetcher))
}

fn list_to_lsp(list: IdeCompletionList) -> CompletionList {
    CompletionList {
        is_incomplete: list.is_incomplete,
        items: list.items.into_iter().map(item_to_lsp).collect(),
    }
}

fn lsp_list_to_ide(list: &CompletionList) -> IdeCompletionList {
    IdeCompletionList {
        is_incomplete: list.is_incomplete,
        items: list.items.iter().map(lsp_item_to_ide).collect(),
    }
}

fn item_to_lsp(item: IdeCompletionItem) -> CompletionItem {
    CompletionItem {
        label: item.label,
        kind: item.kind.map(kind_to_lsp),
        detail: item.detail,
        documentation: item.documentation.map(|markdown| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            })
        }),
        insert_text: item.insert_text,
        insert_text_format: item.insert_text_format.map(insert_text_format_to_lsp),
        sort_text: item.sort_text,
        filter_text: item.filter_text,
        label_details: item.label_details.map(label_details_to_lsp),
        text_edit: item
            .text_edit
            .map(|te| CompletionTextEdit::Edit(text_edit_to_lsp(te))),
        additional_text_edits: item
            .additional_text_edits
            .map(|edits| edits.into_iter().map(text_edit_to_lsp).collect()),
        data: item.data,
        ..Default::default()
    }
}

fn lsp_item_to_ide(item: &CompletionItem) -> IdeCompletionItem {
    IdeCompletionItem {
        label: item.label.clone(),
        kind: None,
        detail: item.detail.clone(),
        documentation: None,
        insert_text: item.insert_text.clone(),
        insert_text_format: None,
        sort_text: item.sort_text.clone(),
        filter_text: item.filter_text.clone(),
        label_details: None,
        text_edit: None,
        additional_text_edits: None,
        data: item.data.clone(),
    }
}

fn kind_to_lsp(kind: IdeCompletionItemKind) -> CompletionItemKind {
    match kind {
        IdeCompletionItemKind::Function => CompletionItemKind::FUNCTION,
        IdeCompletionItemKind::Method => CompletionItemKind::METHOD,
        IdeCompletionItemKind::Variable => CompletionItemKind::VARIABLE,
        IdeCompletionItemKind::Field => CompletionItemKind::FIELD,
        IdeCompletionItemKind::Class => CompletionItemKind::CLASS,
        IdeCompletionItemKind::Enum => CompletionItemKind::ENUM,
        IdeCompletionItemKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        IdeCompletionItemKind::Constant => CompletionItemKind::CONSTANT,
        IdeCompletionItemKind::Module => CompletionItemKind::MODULE,
        IdeCompletionItemKind::Folder => CompletionItemKind::FOLDER,
        IdeCompletionItemKind::Keyword => CompletionItemKind::KEYWORD,
        IdeCompletionItemKind::Text => CompletionItemKind::TEXT,
        IdeCompletionItemKind::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
    }
}

fn insert_text_format_to_lsp(fmt: IdeInsertTextFormat) -> InsertTextFormat {
    match fmt {
        IdeInsertTextFormat::PlainText => InsertTextFormat::PLAIN_TEXT,
        IdeInsertTextFormat::Snippet => InsertTextFormat::SNIPPET,
    }
}

fn label_details_to_lsp(d: IdeCompletionItemLabelDetails) -> CompletionItemLabelDetails {
    CompletionItemLabelDetails {
        detail: d.detail,
        description: d.description,
    }
}

fn text_edit_to_lsp(te: IdeTextEdit) -> TextEdit {
    TextEdit {
        range: range_to_lsp(te.range),
        new_text: te.new_text,
    }
}

fn range_to_lsp(r: IdeRange) -> Range {
    Range {
        start: pos_to_lsp(r.start),
        end: pos_to_lsp(r.end),
    }
}

fn pos_to_lsp(p: IdePosition) -> Position {
    Position {
        line: p.line,
        character: p.character,
    }
}
