//! The doc's types and the crate's agree: every `struct` and `enum` "Data shapes" and "The calls"
//! name is a type here, and every type here is one the doc names.

#![no_std]
#![feature(allocator_api)]

use blaze_own::{SortedSet, String, Vec};
use blaze_wire::WireAlloc;

const DOC: &str = include_str!("../../../docs/distributed.md");

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

/// The `struct` and `enum` names of `text`, as the code blocks write them.
fn names_in(text: &str, alloc: WireAlloc) -> SortedSet<String<WireAlloc>, WireAlloc> {
    let mut names = SortedSet::new_in(alloc);
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
            let mut name = String::new_in(alloc);
            for c in rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_') {
                name.push(c);
            }
            if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_alphabetic()) {
                names.insert(name);
            }
        }
    }
    names
}

/// The doc's two sections, as the ranges the shapes are in.
fn shape_sections() -> &'static str {
    let start = DOC.find("## Data shapes").expect("the doc has no \"Data shapes\"");
    let end = DOC
        .find("## Tests and the remaining acceptance boundary")
        .expect("no end of the doc's shapes");
    &DOC[start..end]
}

#[test]
fn every_type_the_doc_names_is_in_the_crate() {
    let alloc = heap();
    let documented = names_in(shape_sections(), alloc);
    let mut in_crate = SortedSet::new_in(alloc);
    for entry in blaze_wire::vectors::every() {
        in_crate.insert(String::from_str_in(entry.name, alloc));
    }
    assert!(documented.len() >= 40, "the doc's shapes were read as {} types", documented.len());
    let mut missing = Vec::new_in(alloc);
    for name in documented.iter() {
        if !in_crate.contains(name) {
            missing.push(name);
        }
    }
    assert!(missing.is_empty(), "the doc names types the crate does not have: {missing:?}");
}

#[test]
fn every_type_the_crate_has_is_in_the_doc() {
    let alloc = heap();
    let documented = names_in(shape_sections(), alloc);
    let mut in_crate = SortedSet::new_in(alloc);
    for entry in blaze_wire::vectors::every() {
        in_crate.insert(String::from_str_in(entry.name, alloc));
    }
    let payloads = [
        "PrNamespace",
        "ItemKey",
        "EdgeKey",
        "EntryRef",
        "Why",
        "StageEvent",
        "AssignedEvent",
        "RefusedEvent",
        "OutputEvent",
        "DoneEvent",
    ];
    let mut invented = Vec::new_in(alloc);
    for name in in_crate.iter() {
        if !documented.contains(name) && !payloads.contains(&name.as_str()) {
            invented.push(name);
        }
    }
    assert!(invented.is_empty(), "the crate has types the doc does not name: {invented:?}");
}

#[test]
fn the_payload_structs_are_the_ones_the_doc_names() {
    for name in [
        "PrNamespace",
        "ItemKey",
        "EdgeKey",
        "EntryRef",
        "Why",
        "StageEvent",
        "AssignedEvent",
        "RefusedEvent",
        "OutputEvent",
        "DoneEvent",
    ] {
        assert!(DOC.contains(name), "the doc does not name the payload struct {name}");
        assert!(
            blaze_wire::vectors::every().iter().any(|entry| entry.name == name),
            "the crate has no payload struct {name}"
        );
    }
}
