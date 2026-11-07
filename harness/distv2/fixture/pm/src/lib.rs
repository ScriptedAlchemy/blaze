//! A proc macro: its unit is compiled for the macro host, and goes the same way as any other.

use proc_macro::TokenStream;

/// The item it is put on, and a function saying the item's name was seen.
#[proc_macro_attribute]
pub fn seen(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut out: TokenStream = "pub fn seen() -> &'static str { \"seen by the macro\" }".parse().unwrap();
    out.extend(item);
    out
}
