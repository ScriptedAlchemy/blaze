extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote;
use syn::DeriveInput;

#[proc_macro_derive(Describe)]
pub fn describe(input: TokenStream) -> TokenStream {
    let input = syn::parse_str::<DeriveInput>(&input.to_string()).unwrap();
    let name = input.ident;
    let text = shared::label();
    quote!(impl #name { pub fn describe() -> &'static str { #text } }).to_string().parse().unwrap()
}

#[proc_macro]
pub fn host_label(_: TokenStream) -> TokenStream {
    let text = shared::label();
    quote!(#text).to_string().parse().unwrap()
}
