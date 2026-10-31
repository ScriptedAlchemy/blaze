extern crate proc_macro;
#[proc_macro]
pub fn id(i: proc_macro::TokenStream) -> proc_macro::TokenStream { i }
