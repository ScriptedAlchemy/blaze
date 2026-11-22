//! The early identity (docs/v2/plan/design/restore.md, "The entry's key, made before
//! lowering"): it changes with the item's own text, its owner and the macros that wrote it,
//! and with nothing else of its crate.
//!
//! Each test builds two databases from two fixtures and compares the body of the one function
//! named `f` in each.

use blaze_ids::ContentId;

use super::perturb::{body, db, with_dep};
use crate::identity::{early_identity, early_parts};

#[track_caller]
fn early(fixture: &str) -> ContentId {
    let db = db(fixture);
    let def = body(&db, "f");
    early_identity(&db, def).unwrap_or_else(|it| panic!("{it:?}"))
}

#[track_caller]
fn same(before: &str, after: &str) {
    assert_eq!(early(before), early(after), "{before}\n---\n{after}");
}

#[track_caller]
fn differ(before: &str, after: &str) {
    assert_ne!(early(before), early(after), "{before}\n---\n{after}");
}

/// The invariant: an edit of another item of the crate (a body, a signature the body calls,
/// an item that moves it in its file, a sibling method of its impl) leaves the early identity
/// as it was. What the body's steps read of those is a fact of their shapes.
#[test]
fn an_edit_outside_the_item_keeps_the_early_identity() {
    let base = with_dep("pub fn f() -> u8 { g() }\npub fn g() -> u8 { 1 }");
    same(&base, &with_dep("pub fn f() -> u8 { g() }\npub fn g() -> u8 { 2 }"));
    same(&base, &with_dep("pub fn f() -> u8 { g() }\npub fn g() -> u16 { 1 }"));
    same(&base, &with_dep("pub fn h() {}\npub fn f() -> u8 { g() }\npub fn g() -> u8 { 1 }"));
    same(&base, &with_dep("pub fn f() -> u8 { g() }\npub fn g() -> u8 { 1 }\npub struct S;"));
    let method = |h: u8| {
        with_dep(&format!(
            "pub struct S;\nimpl S {{ pub fn f(&self) -> u8 {{ 1 }} pub fn h(&self) -> u8 {{ {h} }} }}"
        ))
    };
    same(&method(2), &method(3));
}

/// The control of the test above: each part of the item's own text, its owner and its place
/// changes the early identity.
#[test]
fn an_edit_of_the_item_or_its_owner_changes_the_early_identity() {
    assert_eq!(item_inputs_change(early), Ok(()));
}

fn item_inputs_change(key: impl Fn(&str) -> ContentId) -> Result<(), usize> {
    let base = with_dep("pub fn f(a: u8) -> u8 { a }");
    let cases = [
        (base.as_str(), with_dep("pub fn f(a: u8) -> u8 { a + 1 }")),
        (base.as_str(), with_dep("pub fn f(mut a: u8) -> u8 { a }")),
        (base.as_str(), with_dep("#[inline]\npub fn f(a: u8) -> u8 { a }")),
        (base.as_str(), with_dep("pub mod m { pub fn f(a: u8) -> u8 { a } }")),
    ];
    for (at, (before, after)) in cases.iter().enumerate() {
        if key(before) == key(after) {
            return Err(at);
        }
    }
    let before = with_dep("pub struct S; pub struct T;\nimpl S { pub fn f(&self) -> u8 { 1 } }");
    let after = with_dep("pub struct S; pub struct T;\nimpl T { pub fn f(&self) -> u8 { 1 } }");
    if key(&before) == key(&after) {
        return Err(cases.len());
    }
    Ok(())
}

#[test]
fn the_oracle_catches_an_early_identity_using_only_its_owner() {
    assert_eq!(item_inputs_change(early), Ok(()));
    let owner_only = |fixture: &str| {
        let db = db(fixture);
        let parts = early_parts(&db, body(&db, "f")).unwrap();
        assert_eq!(parts[1].0, "owner");
        parts[1].1
    };
    assert_eq!(item_inputs_change(owner_only), Err(0), "the body changed under the same owner");
}

/// A body a macro writes is keyed by the call and by each macro of the chain of calls that
/// wrote it, with no expansion read: the definition's tokens, an outer macro's, the call's.
#[test]
fn a_macro_written_body_is_keyed_by_its_calls_and_definitions() {
    let written = |value: u8, g: &str| {
        with_dep(&format!(
            "macro_rules! m {{ ($n:ident) => {{ pub fn $n() -> u8 {{ {value} }} }} }}\n\
             m!(f);\npub fn g() {{ {g} }}"
        ))
    };
    same(&written(1, ""), &written(1, "loop {}"));
    differ(&written(1, ""), &written(2, ""));
    let nested = |inner: &str| {
        with_dep(&format!(
            "macro_rules! inner {{ ($n:ident) => {{ pub fn $n() -> u8 {{ {inner} }} }} }}\n\
             macro_rules! outer {{ ($n:ident) => {{ inner!($n); }} }}\nouter!(f);"
        ))
    };
    differ(&nested("1"), &nested("2"));
    same(&nested("1"), &nested("1"));
}

/// An item of a block has no key of its own: it is keyed with the body it is in.
#[test]
fn an_item_of_a_block_has_no_early_identity() {
    let db = db(&with_dep("pub fn outer() -> u8 { fn f() -> u8 { 1 } f() }"));
    let def = body(&db, "f");
    assert!(early_parts(&db, def).is_err());
}
