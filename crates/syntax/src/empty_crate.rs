//! The complete source check for a library with no items.
//!
//! This is a positive proof for a small, closed case, not an approximation of a
//! compiler's checks on arbitrary Rust. Every node and token must be accounted
//! for. In particular, cfg attributes, macro calls and parser recovery cannot
//! make a crate appear empty without their own semantic checks.

use crate::{AstNode, Edition, NodeOrToken, SourceFile, SyntaxKind, ast};

/// An empty library has no bodies, signatures, definitions or exports to check
/// or lower. Its implicit language library is still chosen by `no_std`.
#[derive(Debug, PartialEq, Eq)]
pub struct EmptyCrate {
    no_std: bool,
}

impl EmptyCrate {
    pub fn no_std(&self) -> bool {
        self.no_std
    }
}

/// Return a proof only for syntax whose entire meaning is known here. A refusal
/// says nothing about whether a fuller compiler would accept the source.
pub fn check(text: &str, edition: Edition) -> Result<EmptyCrate, &'static str> {
    let parsed = SourceFile::parse(text, edition);
    if !parsed.errors().is_empty() {
        return Err("the source has syntax errors");
    }
    let mut result = EmptyCrate { no_std: false };
    for child in parsed.syntax_node().children_with_tokens() {
        match child {
            NodeOrToken::Token(token) => match token.kind() {
                SyntaxKind::WHITESPACE | SyntaxKind::COMMENT | SyntaxKind::SHEBANG => {}
                _ => return Err("the source contains an unchecked token"),
            },
            NodeOrToken::Node(node) => match ast::AnyAttr::cast(node) {
                Some(ast::AnyAttr::Attr(attr))
                    if attr.kind().is_inner()
                        && attr.as_simple_atom().as_deref() == Some("no_std") =>
                {
                    result.no_std = true;
                }
                Some(ast::AnyAttr::DocComment(doc)) if doc.kind().is_inner() => {}
                Some(_) => return Err("the crate has an unchecked attribute"),
                None => return Err("the crate contains items or recovered syntax"),
            },
        }
    }
    Ok(result)
}

/// Crate names are not Rust identifiers: leading digits and keywords are legal.
/// The compiler permits only Unicode alphanumeric characters and underscores.
pub fn valid_crate_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_libraries_account_for_all_source_syntax() {
        for edition in Edition::iter() {
            for (source, no_std) in [
                ("", false),
                (" \n// comment\n/* nested /* block */ comment */", false),
                ("//! crate docs\n/*! more docs */\n", false),
                ("#!/usr/bin/env rust-script\n", false),
                ("#![no_std]", true),
                ("#![no_std]\n#![no_std]\n//! docs\n", true),
            ] {
                let checked = check(source, edition).unwrap();
                assert_eq!(checked.no_std(), no_std, "{edition}: {source}");
            }
        }
    }

    #[test]
    fn neither_recovery_nor_attributes_prove_empty_crate_acceptance() {
        for source in [
            "pub fn f() {}",
            "fn broken() {",
            "pub type T = Missing;",
            "mod missing;",
            "extern crate missing;",
            "use missing::*;",
            "compile_error!(\"not empty\");",
            "include!(\"items.rs\");",
            "#[cfg(any())] fn f(_: Missing) {}",
            "#![cfg(any())]\nfn f(_: Missing) {}",
            "#![cfg_attr(all(), no_std)]",
            "#![feature(no_core)]\n#![no_core]",
            "#![no_std(unchecked)]",
            "#![no_std = \"unchecked\"]",
            "#![no_std::unchecked]",
            "#![crate_name = \"different\"]",
            "#![crate_type = \"bin\"]",
            "#![deny(warnings)]",
            "#![unknown]",
            "#[no_std]",
            "/// outer docs without an item",
            "/** outer docs without an item */",
            "/* unterminated",
            "#![no_std]\n}",
            "#!/usr/bin/env cargo\n---\n[dependencies]\n---\n",
        ] {
            assert!(check(source, Edition::CURRENT).is_err(), "{source}");
        }
    }

    #[test]
    fn crate_names_follow_the_compiler_rule_not_identifier_syntax() {
        for name in ["a", "_", "0abc", "type", "é", "a_b2"] {
            assert!(valid_crate_name(name), "{name}");
        }
        for name in ["", "a-b", "a/b", "a.b", "r#type", "a b", "a\nb", "a\\b"] {
            assert!(!valid_crate_name(name), "{name}");
        }
    }
}
