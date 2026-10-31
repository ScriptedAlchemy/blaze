#[cfg(all(feature = "host", feature = "target"))]
compile_error!("host and target features were unified");

#[cfg(feature = "host")]
pub fn label() -> &'static str {
    "host-only"
}

#[cfg(feature = "target")]
pub fn label() -> &'static str {
    "target-only"
}
