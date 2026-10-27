//! The doc's types and the crate's agree: every `struct` and `enum` "Data shapes" and section 8
//! name is a type here, and every type here is one the doc names.
//!
//! This is the check that a type the doc adds is not silently missing from the crate, and that the
//! crate does not invent a type the doc does not have.

use std::collections::BTreeSet;

/// The doc, read from the workspace the crate is in.
fn doc() -> String {
    let path = format!("{}/../../docs/distributed.md", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("no {path}: {e}"))
}

/// The `struct` and `enum` names of `text`, as the code blocks write them.
fn names_in(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim_start();
        let rest = line
            .strip_prefix("pub ")
            .unwrap_or(line)
            .strip_prefix("struct ")
            .or_else(|| line.strip_prefix("pub ").unwrap_or(line).strip_prefix("enum "))
            .or_else(|| line.strip_prefix("struct "))
            .or_else(|| line.strip_prefix("enum "));
        if let Some(rest) = rest {
            let name: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_alphabetic()) {
                names.insert(name);
            }
        }
    }
    names
}

/// The doc's two sections, as the ranges the shapes are in.
fn shape_sections() -> String {
    let doc = doc();
    let start = doc.find("## Data shapes").expect("the doc has no \"Data shapes\"");
    let end = doc.find("What the server keeps on its own disk").expect("no end of \"Data shapes\"");
    let calls = doc.find("### The shapes of the calls").expect("the doc has no call shapes");
    let calls_end = doc.find("## 9. Upstream's objections").expect("no section 9");
    format!("{}\n{}", &doc[start..end], &doc[calls..calls_end])
}

#[test]
fn every_type_the_doc_names_is_in_the_crate() {
    let documented = names_in(&shape_sections());
    let in_crate: BTreeSet<String> =
        blaze_wire::vectors::every().iter().map(|entry| entry.name.to_owned()).collect();
    assert!(documented.len() >= 80, "the doc's shapes were read as {} types", documented.len());
    let missing: Vec<&String> = documented.difference(&in_crate).collect();
    assert!(missing.is_empty(), "the doc names types the crate does not have: {missing:?}");
}

#[test]
fn every_type_the_crate_has_is_in_the_doc() {
    let documented = names_in(&shape_sections());
    let in_crate: BTreeSet<String> =
        blaze_wire::vectors::every().iter().map(|entry| entry.name.to_owned()).collect();
    let invented: Vec<&String> = in_crate.difference(&documented).collect();
    // The payload structs of section 8's enum list are named in a sentence, not in a code block,
    // so they are checked by their own list.
    let payloads: BTreeSet<&str> = [
        "GitRef",
        "RootRef",
        "PrNamespace",
        "ItemKey",
        "EdgeKey",
        "EntryRef",
        "Why",
        "FileObservation",
        "DirObservation",
        "AbsentObservation",
        "LinkObservation",
        "StageEvent",
        "AssignedEvent",
        "PublishedEvent",
        "ProgramEvent",
        "RefusedEvent",
        "OutputEvent",
        "DoneEvent",
    ]
    .into_iter()
    .collect();
    let invented: Vec<&&String> =
        invented.iter().filter(|name| !payloads.contains(name.as_str())).collect();
    assert!(invented.is_empty(), "the crate has types the doc does not name: {invented:?}");
}

#[test]
fn the_payload_structs_are_the_ones_the_doc_names() {
    let doc = doc();
    for name in [
        "GitRef",
        "RootRef",
        "PrNamespace",
        "ItemKey",
        "EdgeKey",
        "EntryRef",
        "Why",
        "FileObservation",
        "DirObservation",
        "AbsentObservation",
        "LinkObservation",
        "StageEvent",
        "AssignedEvent",
        "PublishedEvent",
        "ProgramEvent",
        "RefusedEvent",
        "OutputEvent",
        "DoneEvent",
    ] {
        assert!(doc.contains(name), "the doc does not name the payload struct {name}");
        assert!(
            blaze_wire::vectors::every().iter().any(|entry| entry.name == name),
            "the crate has no payload struct {name}"
        );
    }
}
