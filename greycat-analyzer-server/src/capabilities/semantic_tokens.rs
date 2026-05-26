//! Thin converter from the IDE-shape `analysis::ide::semantic_tokens`
//! ADT to `lsp_types::SemanticTokens`. The flat `Vec<u32>` produced by
//! the analyzer is regrouped into `SemanticToken` quintuples for the
//! wire shape.

use greycat_analyzer_analysis::ide::semantic_tokens::{
    SemanticTokens as IdeSemanticTokens, semantic_tokens as semantic_tokens_inner,
};
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{SemanticToken, SemanticTokenType, SemanticTokens};

/// Token-type legend the server registers with the client. Mirrors
/// the indices used by [`greycat_analyzer_analysis::ide::semantic_tokens::SEMANTIC_TOKEN_TYPES`].
pub const SEMANTIC_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::FUNCTION,
    SemanticTokenType::TYPE,
    SemanticTokenType::ENUM,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::COMMENT,
    SemanticTokenType::KEYWORD,
];

pub fn semantic_tokens(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> SemanticTokens {
    to_lsp(semantic_tokens_inner(text, lib, root, encoding))
}

fn to_lsp(t: IdeSemanticTokens) -> SemanticTokens {
    let mut data = Vec::with_capacity(t.data.len() / 5);
    for chunk in t.data.chunks(5) {
        let [
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset,
        ] = *chunk
        else {
            continue;
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset,
        });
    }
    SemanticTokens {
        result_id: None,
        data,
    }
}
