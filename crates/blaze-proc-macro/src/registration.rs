//! D52's registration ABI, checked by the execution engine's exact Wasm validator.

use core::fmt;
use wasmtime::wasmparser;

#[derive(Debug)]
pub enum WasmRegistrationError {
    Invalid(wasmparser::BinaryReaderError),
    NotCore,
    Missing,
    Ambiguous,
    WrongType,
}

impl fmt::Display for WasmRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "invalid WebAssembly: {error}"),
            Self::NotCore => f.write_str("D52 requires a core WebAssembly module"),
            Self::Missing => f.write_str("no exported __rustc_proc_macro_decls_* registration global"),
            Self::Ambiguous => f.write_str("multiple __rustc_proc_macro_decls_* registration exports"),
            Self::WrongType => f.write_str("the __rustc_proc_macro_decls_* registration export must be an i32 global"),
        }
    }
}

/// The address passed to D52's shim comes from the validator's export type table.
/// Both the bootstrap producer and the consuming module task use this check.
pub fn proc_macro_decls<T>(wasm: &[u8], use_name: impl FnOnce(&str) -> T) -> Result<T, WasmRegistrationError> {
    let types = wasmparser::Validator::new().validate_all(wasm).map_err(WasmRegistrationError::Invalid)?;
    let exports = types.as_ref().core_exports().ok_or(WasmRegistrationError::NotCore)?;
    let mut declarations = exports.filter(|(name, _)|
        name.strip_prefix("__rustc_proc_macro_decls_").is_some_and(|suffix| !suffix.is_empty()));
    let (name, ty) = declarations.next().ok_or(WasmRegistrationError::Missing)?;
    if declarations.next().is_some() { return Err(WasmRegistrationError::Ambiguous); }
    if !matches!(ty, wasmparser::types::EntityType::Global(global) if global.content_type == wasmparser::ValType::I32) {
        return Err(WasmRegistrationError::WrongType);
    }
    Ok(use_name(name))
}
