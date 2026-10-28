extern crate proc_macro;

use proc_macro::{TokenStream, TokenTree};

#[proc_macro_derive(Describe)]
pub fn describe(input: TokenStream) -> TokenStream {
    let mut tokens = input.into_iter();
    while let Some(token) = tokens.next() {
        if matches!(token, TokenTree::Ident(ref ident) if ident.to_string() == "struct") {
            let name = tokens.next().unwrap().to_string();
            let label = shared::label();
            return format!("impl {name} {{ pub fn describe() -> &'static str {{ \"{label}\" }} }}")
                .parse()
                .unwrap();
        }
    }
    TokenStream::new()
}

#[proc_macro]
pub fn host_label(_: TokenStream) -> TokenStream {
    format!("{:?}", shared::label()).parse().unwrap()
}
