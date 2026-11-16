//! `#[blaze::noalloc]` and `#[blaze::nonblocking]` over the stand-in toolchain
//! (docs/v2/plan/design/effects.md).

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
};
use crate::{
    PURE, Rule, check_generic,
    effect_attrs::{NOALLOC, NONBLOCKING},
};

#[test]
fn a_parameter_refusal_names_the_first_instantiated_field_path() {
    let db = hir_expand::fixture::db(
        r#"
//- minicore: sized, copy, phantom_data
//- /alloc.rs crate:alloc
pub struct RawVec<T> {
    bytes: *const u8,
    marker: core::marker::PhantomData<T>,
}
pub struct Vec<T> {
    buf: RawVec<T>,
}
//- /lib.rs crate:kernel deps:alloc
use core::marker::PhantomData;

pub mod vfs { pub struct View { value: u8 } }
pub mod other { pub struct View { value: u8 } }

pub struct Idx<T> { raw: u32, of: PhantomData<fn() -> T> }
pub struct Jobs { retired: alloc::Vec<Idx<vfs::View>> }
pub struct Daemon { jobs: Jobs, later: Idx<other::View> }

pub fn run(daemon: &mut Daemon, callback: fn()) {}
pub fn input_number(scalar: u8, daemon: &Daemon) {}
pub fn scalar(values: &alloc::Vec<u8>) {}

pub enum Choice { Empty, Value { callback: fn() } }
pub fn variant(choice: &Choice) {}

pub struct Node { next: *const Node, callback: fn() }
pub fn recursive(node: &Node) {}
"#,
    )
    .into_db();
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        let run = fn_named(&db, krate, "run");
        let refusal = check_generic(&db, run, &NONBLOCKING).unwrap_err();
        assert_eq!(refusal.rule, Rule::Unchecked, "{}", refusal.said());
        let (_, path) = refusal.what.split_once("parameter path: ").unwrap();
        let mut rest = path;
        for part in [
            "parameter 0:",
            "kernel::Daemon",
            "field `jobs`:",
            "kernel::Jobs",
            "field `retired`:",
            "alloc::Vec<kernel::Idx<kernel::vfs::View>>",
            "field `buf`:",
            "alloc::RawVec<kernel::Idx<kernel::vfs::View>>",
            "field `marker`:",
            "type argument: `kernel::Idx<kernel::vfs::View>`",
            "field `of`:",
            "type argument: `fn() -> kernel::vfs::View`",
        ] {
            let Some((_, tail)) = rest.split_once(part) else {
                panic!("missing {part:?} in {path}")
            };
            rest = tail;
        }
        assert!(!path.contains("other::View"), "{path}");
        assert!(!path.contains("parameter 1:"), "{path}");

        let input = fn_named(&db, krate, "input_number");
        let refusal = check_generic(&db, input, &NONBLOCKING).unwrap_err();
        assert!(refusal.what.contains("parameter path: parameter 1:"), "{}", refusal.said());

        for (name, expected) in [
            ("variant", "variant `Value` field `callback`: `fn()`"),
            ("recursive", "field `callback`: `fn()`"),
        ] {
            let refusal = check_generic(&db, fn_named(&db, krate, name), &NONBLOCKING).unwrap_err();
            assert!(refusal.what.contains(expected), "{name}: {}", refusal.said());
        }
        check_generic(&db, fn_named(&db, krate, "scalar"), &NONBLOCKING).unwrap();
    });
}

/// The pinned core's `unsafe` precondition checks (`unchecked_add`'s, under every
/// `Range<usize>` loop) build their message with `fmt::Arguments::from_str`, which transmutes a
/// `usize` into the `NonNull<rt::Argument>` of arguments holding function pointers. A pointer
/// made of a number has no provenance, so no code is reached through it: the check passes. A
/// function pointer made of a number is still refused.
#[test]
fn a_pointer_made_of_a_number_reaches_no_code() {
    let db = db_with(
        r#"
//- /lib.rs crate:kernel deps:std
pub struct Argument {
    formatter: fn(&u8) -> bool,
}

/// `core::fmt::Arguments` at the pin: a message's length packed into the pointer to its
/// arguments.
pub struct Message {
    args: *const Argument,
}

fn from_len(len: usize) -> Message {
    Message { args: unsafe { core::mem::transmute(len) } }
}

/// `unchecked_add`'s precondition check: the message, then an abort.
fn precondition_check(a: usize, b: usize) {
    if a >= b {
        let _message = from_len(41);
        core::panicking::panic("unsafe precondition(s) violated: unchecked_add");
    }
}

#[blaze::noalloc]
#[blaze::nonblocking]
pub fn count(n: usize) -> usize {
    let mut i = 0;
    while i < n {
        precondition_check(i, n);
        i += 1;
    }
    i
}

#[blaze::noalloc]
#[blaze::nonblocking]
pub fn jump(at: usize) -> fn() {
    unsafe { core::mem::transmute(at) }
}
"#,
    );
    let krate = crate_named(&db, "kernel");
    let (count, jump) = (fn_named(&db, krate, "count"), fn_named(&db, krate, "jump"));
    blaze_query::db::attach(&db, || {
        for effect in [&NOALLOC, &NONBLOCKING] {
            if let Err(refusal) = check_generic(&db, count, effect) {
                panic!("`count` should be {}, but {}", effect.name, refusal.said());
            }
            let Err(refusal) = check_generic(&db, jump, effect) else {
                panic!("`jump` makes a function pointer of a number, so it is not {}", effect.name);
            };
            let said = refusal.said();
            assert!(
                said.contains("transmutes `usize`")
                    && said.contains("a function pointer or vtable made of bytes"),
                "{said}"
            );
        }
    });
}

/// At the pin a `Vec`'s buffer is a `NonNull<u8>`, whose field is the pattern type
/// `pattern_type!(*const u8 is !null)`: a raw pointer in a toolchain type's field, looked
/// through as the toolchain's own, not one the function takes (blaze checking blaze, E4:
/// every `Compute::compute`, which takes `&mut Reads` and its `Vec<u8>`, was refused). A raw
/// pointer of the crate's own is still refused.
#[test]
fn a_raw_pointer_in_a_toolchain_pattern_type_is_the_toolchains() {
    let db = hir_expand::fixture::db(
        r#"
//- minicore: sized, copy, non_null
//- /lib.rs crate:kernel
pub struct Keys {
    buf: core::ptr::NonNull<u8>,
    len: usize,
}

pub struct Raw {
    buf: *const u8,
}

pub fn keys(keys: &mut Keys, input: &u8) -> u8 {
    let _ = keys;
    *input
}

pub fn raw(raw: &mut Raw, input: &u8) -> u8 {
    let _ = raw;
    *input
}
"#,
    )
    .into_db();
    let krate = crate_named(&db, "kernel");
    let (keys, raw) = (fn_named(&db, krate, "keys"), fn_named(&db, krate, "raw"));
    blaze_query::db::attach(&db, || {
        if let Err(refusal) = check_generic(&db, keys, &PURE) {
            panic!(
                "`keys` takes a `NonNull` of the toolchain, so it should be pure, but {}",
                refusal.said()
            );
        }
        let Err(refusal) = check_generic(&db, raw, &PURE) else {
            panic!("`raw` takes a raw pointer of its own, so it is not pure");
        };
        let said = refusal.said();
        assert!(said.contains("takes the raw pointer `*const u8`"), "{said}");
    });
}
