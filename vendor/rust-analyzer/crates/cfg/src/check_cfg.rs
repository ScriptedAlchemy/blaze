//! Rust's `--check-cfg` specification grammar. Validation is separate from
//! evaluating cfgs: these declarations control warnings, never active source.

use syntax::{AstToken as _, Edition, LexedStr, SyntaxKind, ast};

enum Meta<'a> {
    Name,
    List(&'a str, Vec<Meta<'a>>),
    Value,
}

pub fn valid(text: &str, edition: Edition) -> bool {
    fn string(text: &str) -> bool {
        let hashes =
            text.strip_prefix('r').map(|rest| rest.bytes().take_while(|&b| b == b'#').count());
        if !text.ends_with(&format!("\"{}", "#".repeat(hashes.unwrap_or(0)))) {
            return false;
        }
        let source = format!("const _: &str = {text};");
        let parsed = ast::SourceFile::parse(&source, Edition::Edition2021);
        parsed.errors().is_empty()
            && parsed
                .syntax_node()
                .descendants_with_tokens()
                .filter_map(|it| it.into_token())
                .find_map(ast::String::cast)
                .is_some_and(|it| it.value().is_ok())
    }
    fn meta<'a>(tokens: &[(SyntaxKind, &'a str)], at: &mut usize, depth: u8) -> Option<Meta<'a>> {
        if depth > 3 {
            return None;
        }
        let &(kind, text) = tokens.get(*at)?;
        *at += 1;
        if kind == SyntaxKind::STRING {
            return string(text).then_some(Meta::Value);
        }
        if kind != SyntaxKind::IDENT && !matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
        {
            return None;
        }
        let text = text.strip_prefix("r#").unwrap_or(text);
        if matches!(text, "self" | "Self" | "super" | "crate") {
            return None;
        }
        if tokens.get(*at).map(|it| it.0) != Some(SyntaxKind::L_PAREN) {
            return Some(Meta::Name);
        }
        *at += 1;
        let mut values = Vec::new();
        while tokens.get(*at).map(|it| it.0) != Some(SyntaxKind::R_PAREN) {
            values.push(meta(tokens, at, depth + 1)?);
            if tokens.get(*at).map(|it| it.0) != Some(SyntaxKind::COMMA) {
                break;
            }
            *at += 1;
        }
        if tokens.get(*at).map(|it| it.0) != Some(SyntaxKind::R_PAREN) {
            return None;
        }
        *at += 1;
        Some(Meta::List(text, values))
    }
    let lexed = LexedStr::new(edition, text);
    if lexed.errors().next().is_some() {
        return false;
    }
    let tokens: Vec<_> = (0..lexed.len())
        .filter(|&i| !lexed.kind(i).is_trivia())
        .map(|i| (lexed.kind(i), lexed.text(i)))
        .collect();
    let mut at = 0;
    let Some(Meta::List("cfg", mut args)) = meta(&tokens, &mut at, 0) else { return false };
    if at != tokens.len() {
        return false;
    }
    if matches!(&*args, [Meta::List("any", items)] if items.is_empty()) {
        return true;
    }
    if let Some(Meta::List("values", values)) = args.last() {
        if args.len() == 1 {
            return false;
        }
        if !matches!(&**values, [Meta::List("any", items)] if items.is_empty())
            && !values.iter().all(|it| {
                matches!(it, Meta::Value)
                    || matches!(it, Meta::List("none", items) if items.is_empty())
            })
        {
            return false;
        }
        args.pop();
    }
    args.iter().all(|it| matches!(it, Meta::Name))
}

#[cfg(test)]
mod tests {
    #[test]
    fn specifications_follow_rustc_meta_and_value_rules() {
        for text in [
            "cfg()",
            "cfg(foo)",
            "cfg(foo,bar,)",
            "cfg(any())",
            "cfg(r#type)",
            "cfg(true,false)",
            "r#cfg(foo)",
            "cfg(foo,values())",
            "cfg(foo,values(any()))",
            "cfg(foo,values(none(),none()))",
            "cfg(foo,values(\"bar\",none()))",
            "cfg(foo,values(r#\"bar\"#))",
        ] {
            assert!(super::valid(text, syntax::Edition::Edition2021), "{text}");
        }
        for text in [
            "",
            "foo",
            "cfg(foo",
            "cfg(type)",
            "cfg(r#self)",
            "cfg(r#Self)",
            "cfg(r#super)",
            "cfg(r#crate)",
            "cfg(foo,any())",
            "cfg(values())",
            "cfg(foo,values(\"bar\"),bar)",
            "cfg(foo,values(any(),none()))",
            "cfg(foo,values(\"bad\\q\"))",
            "cfg(foo,values(\"x\"suffix))",
            "cfg(foo) extra",
        ] {
            assert!(!super::valid(text, syntax::Edition::Edition2021), "{text}");
        }
        assert!(super::valid("cfg(async)", syntax::Edition::Edition2015));
        assert!(!super::valid("cfg(async)", syntax::Edition::Edition2021));
        assert!(super::valid("cfg(gen)", syntax::Edition::Edition2021));
        assert!(!super::valid("cfg(gen)", syntax::Edition::Edition2024));
    }
}
