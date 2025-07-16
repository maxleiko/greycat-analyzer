use greycat_analyzer_core::CstParser;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn parse_cst(source: &str) -> Result<JsValue, JsValue> {
    let mut parser = CstParser::new(source);
    match parser.parse_module(source) {
        Ok(module) => Ok(serde_wasm_bindgen::to_value(&module)?),
        Err(err) => {
            let err = err.to_source_error(source);
            Err(serde_wasm_bindgen::to_value(&err)?)
        }
    }
}
