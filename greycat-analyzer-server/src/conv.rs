//! LSP <-> byte-offset conversions and small geometry helpers. The
//! implementation lives in [`greycat_analyzer_analysis::conv`] so both
//! the LSP capability layer and the analysis-side ide services share
//! a single definition. This module is the server-side facade.

pub(crate) use greycat_analyzer_analysis::conv::{
    byte_range_to_lsp, byte_to_position, position_to_byte, ranges_overlap,
};
