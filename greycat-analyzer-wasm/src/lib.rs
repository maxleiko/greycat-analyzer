use greycat_analyzer_core::{cst, tokenize};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn parse_cst(source: &str) -> Result<JsValue, JsValue> {
    let tokens = tokenize(source);
    let module = cst::parse(&tokens);
    Ok(serde_wasm_bindgen::to_value(&module)?)
}
