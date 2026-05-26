//! Thin converter from `analysis::ide::signature_help` ADTs to
//! `lsp_types::SignatureHelp`.

use greycat_analyzer_analysis::ide::signature_help::{
    ParameterInformation as IdeParameterInformation, SignatureHelp as IdeSignatureHelp,
    SignatureInformation as IdeSignatureInformation, signature_help as signature_help_inner,
};
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position,
    SignatureHelp, SignatureInformation,
};

pub fn signature_help(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    encoding: SourceEncoding,
) -> Option<SignatureHelp> {
    signature_help_inner(text, lib, root, pos, encoding).map(to_lsp)
}

fn to_lsp(h: IdeSignatureHelp) -> SignatureHelp {
    SignatureHelp {
        signatures: h.signatures.into_iter().map(sig_to_lsp).collect(),
        active_signature: Some(h.active_signature),
        active_parameter: Some(h.active_parameter),
    }
}

fn sig_to_lsp(s: IdeSignatureInformation) -> SignatureInformation {
    SignatureInformation {
        label: s.label,
        documentation: s.documentation.map(|d| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: d,
            })
        }),
        parameters: Some(s.parameters.into_iter().map(param_to_lsp).collect()),
        active_parameter: Some(s.active_parameter),
    }
}

fn param_to_lsp(p: IdeParameterInformation) -> ParameterInformation {
    ParameterInformation {
        label: ParameterLabel::LabelOffsets([p.label_start, p.label_end]),
        documentation: p.documentation.map(|d| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: d,
            })
        }),
    }
}
