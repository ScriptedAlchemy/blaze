//! Three proc macros: a derive and an attribute that read only their input, and a derive that
//! reads a file of the package it is used in while it expands, as `rust-embed` does.

use proc_macro::{TokenStream, TokenTree};

fn name_of(input: &TokenStream) -> String {
    let mut tokens = input.clone().into_iter();
    while let Some(token) = tokens.next() {
        if matches!(&token, TokenTree::Ident(it) if it.to_string() == "struct" || it.to_string() == "fn")
        {
            return tokens.next().map(|it| it.to_string()).unwrap_or_default();
        }
    }
    String::new()
}

/// `impl Name { pub fn describe() -> &'static str }`, from the name alone.
#[proc_macro_derive(Describe)]
pub fn describe(input: TokenStream) -> TokenStream {
    let name = name_of(&input);
    let suffix = macro_producer::suffix();
    format!("impl {name} {{ pub fn describe() -> &'static str {{ \"{name} was described {suffix}\" }} }}")
        .parse()
        .unwrap()
}

/// Leaves the function as it is and adds `<name>_note()`, which returns the attribute's text.
#[proc_macro_attribute]
pub fn note(args: TokenStream, item: TokenStream) -> TokenStream {
    let name = name_of(&item);
    let text = args.to_string().replace('"', "");
    let extra: TokenStream =
        format!("pub fn {name}_note() -> &'static str {{ \"{text}\" }}").parse().unwrap();
    item.into_iter().chain(extra).collect()
}

/// `impl Name { pub fn embedded() -> &'static str }`, with what `embedded.txt` of the package that uses it
/// says: it reads the file where the macro is run, so what it expands to is not a function of
/// its input.
#[proc_macro_derive(Embedded)]
pub fn embedded(input: TokenStream) -> TokenStream {
    let name = name_of(&input);
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let text = std::fs::read_to_string(format!("{dir}/embedded.txt")).unwrap_or_default();
    let text = text.trim().replace('"', "");
    format!("impl {name} {{ pub fn embedded() -> &'static str {{ \"{text}\" }} }}").parse().unwrap()
}

/// Observes a machine path without putting the path itself in the expansion. Normalizing
/// CARGO_MANIFEST_DIR in a lookup key would incorrectly reuse another machine's length.
#[proc_macro_derive(ManifestLength)]
pub fn manifest_length(input: TokenStream) -> TokenStream {
    let name = name_of(&input);
    let length = std::env::var("CARGO_MANIFEST_DIR").unwrap().len();
    format!("impl {name} {{ pub fn manifest_length() -> usize {{ {length} }} }}").parse().unwrap()
}
