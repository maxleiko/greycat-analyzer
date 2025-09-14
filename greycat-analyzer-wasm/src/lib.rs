use greycat_analyzer_core::{
    bumpalo::Bump,
    cst::{self, ParserCtx},
    tokenize,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn parse_cst(source: &str) -> Result<JsValue, JsValue> {
    let arena = Bump::new();
    let ctx = ParserCtx {
        arena: &arena,
        tokens: &tokenize(source),
    };
    let module = cst::parse(ctx);
    Ok(serde_wasm_bindgen::to_value(&module)?)
}
