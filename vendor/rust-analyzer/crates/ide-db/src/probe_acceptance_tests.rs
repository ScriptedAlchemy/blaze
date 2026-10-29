//! `hir::Crate::probe_verdict` on fixture standard libraries whose stability marks
//! are written as the release's library source writes them.

use hir::ProbeVerdict;
use test_fixture::WithFixture;

use crate::RootDatabase;

const LIBRARY: &str = r#"
//- /proc_macro.rs crate:proc_macro
#![stable(feature = "proc_macro_lib", since = "1.15.0")]
#[stable(feature = "proc_macro_lib2", since = "1.29.0")]
pub struct Span(u32);
impl Span {
    #[stable(feature = "proc_macro_span_location", since = "1.88.0")]
    pub fn start(&self) -> Span { Span(0) }
    #[stable(feature = "proc_macro_span_location", since = "1.88.0")]
    pub fn line(&self) -> usize { 0 }
    #[stable(feature = "proc_macro_span_location", since = "1.88.0")]
    pub fn shift(&self, by: usize) -> Span { Span(by as u32) }
    #[unstable(feature = "proc_macro_span", issue = "54725")]
    pub fn byte_range(&self) -> usize { 0 }
    #[stable(feature = "proc_macro_span_location", since = "1.88.0")]
    fn private(&self) -> usize { 0 }
    pub fn unmarked(&self) -> usize { 0 }
    #[stable(feature = "proc_macro_span_location", since = "1.88.0")]
    pub fn mutate(&mut self) -> usize { 0 }
}
#[stable(feature = "proc_macro_lib2", since = "1.29.0")]
pub struct Name(u32);
impl Name {
    #[stable(feature = "proc_macro_lib2", since = "1.29.0")]
    pub fn into_line(self) -> usize { 0 }
    #[stable(feature = "proc_macro_lib2", since = "1.29.0")]
    pub fn len(&self) -> usize { 0 }
    #[stable(feature = "proc_macro_lib2", since = "1.29.0")]
    pub fn plus(self, by: usize) -> usize { by }
}
#[unstable(feature = "proc_macro_internals", issue = "27812")]
pub struct Hidden;
#[unstable(feature = "proc_macro_soft", issue = "1", soft)]
pub struct Soft;
#[stable(feature = "proc_macro_lib2", since = "1.29.0")]
pub mod unmarked_parent {
    pub struct Inner;
}
"#;

fn verdict(probe: &str) -> ProbeVerdict {
    let fixture = format!("{LIBRARY}\n//- /lib.rs crate:probe deps:proc_macro cfg:probe\n{probe}");
    let (db, _) = RootDatabase::with_many_files(&fixture);
    let krate = hir::Crate::all(&db)
        .into_iter()
        .find(|it| it.display_name(&db).is_some_and(|name| name.to_string() == "probe"))
        .unwrap();
    krate.probe_verdict(&db)
}

fn accepted(probe: &str) {
    match verdict(probe) {
        ProbeVerdict::Accepted { sysroot_crates } => {
            assert_eq!(sysroot_crates, ["core", "proc_macro"], "{probe}")
        }
        other => panic!("{probe}: {other:?}"),
    }
}

fn undecided(probe: &str) {
    assert!(matches!(verdict(probe), ProbeVerdict::Undecided(_)), "{probe}: {:?}", verdict(probe));
}

#[test]
fn stable_inherent_methods_of_stable_types_are_accepted() {
    accepted(
        "#![no_std]\nextern crate proc_macro;\nuse proc_macro::Span;\n\
         pub fn line(this: &Span) -> usize { this.line() }\n\
         pub fn start(this: &Span) -> Span { this.start() }\n\
         fn chain(this: Span) -> usize { this.start().shift(this.line()).line() }\n",
    );
    accepted(
        "#![cfg_attr(probe, no_std)]\nextern crate proc_macro;\n\
         pub fn line(this: proc_macro::Span) -> usize { this.line() }\n",
    );
    accepted(
        "#![no_std]\nextern crate proc_macro;\nuse proc_macro::{Name, Span};\n\
         pub fn moved(name: Name) -> usize { name.into_line() }\n\
         pub fn borrowed(name: Name) -> usize { name.len() }\n\
         pub fn copied(this: &Span) -> Span { this.start() }\n",
    );
}

#[test]
fn unstable_paths_are_rejected_and_other_unstable_uses_undecided() {
    let hidden = "#![no_std]\nextern crate proc_macro;\nuse proc_macro::Hidden;\n";
    assert!(matches!(verdict(hidden), ProbeVerdict::Rejected(_)), "{:?}", verdict(hidden));
    // rustc may pick a stable trait method over an unstable inherent one.
    undecided(
        "#![no_std]\nextern crate proc_macro;\nuse proc_macro::Span;\n\
         pub fn range(this: &Span) -> usize { this.byte_range() }\n",
    );
    // A soft-unstable path is a lint, which `--cap-lints=allow` silences.
    undecided("#![no_std]\nextern crate proc_macro;\nuse proc_macro::Soft;\n");
    undecided("#![no_std]\nextern crate proc_macro;\nuse proc_macro::unmarked_parent::Inner;\n");
}

#[test]
fn privacy_types_moves_and_unmarked_items_are_not_guessed() {
    for body in [
        "pub fn f(this: &Span) -> usize { this.private() }",
        "pub fn f(this: &Span) -> usize { this.unmarked() }",
        "pub fn f(this: &mut Span) -> usize { this.mutate() }",
        "pub fn f(this: &Span) -> u32 { this.line() }",
        "pub fn f(this: &Span) -> usize { this.absent() }",
        "pub fn f(this: &Span) -> usize { this.shift(1).line() }",
        "pub fn f(this: &Span) -> usize { 1 }",
        "pub fn f(this: &Name) -> usize { this.into_line() }",
        "pub fn f(name: Name) -> usize { name.plus(name.len()) }",
        "pub fn f(this: &Span) -> usize { let x = this.line(); x }",
        "pub fn f<T>(this: &Span) -> usize { this.line() }",
        "#[inline] pub fn f(this: &Span) -> usize { this.line() }",
        "pub unsafe fn f(this: &Span) -> usize { this.line() }",
        "pub fn f(this: &Span) -> &Span { this }",
        "pub fn f(this: &Span) -> usize { this.line() }\npub fn f(this: &Span) -> usize { 0 }",
        "pub struct Local;",
        "mod inner {}",
    ] {
        undecided(&format!(
            "#![no_std]\nextern crate proc_macro;\nuse proc_macro::{{Name, Span}};\n{body}\n"
        ));
    }
    for crate_attr in ["#![no_std]\n#![allow(unused)]", "#![cfg_attr(any(probe), no_std)]"] {
        undecided(&format!("{crate_attr}\nextern crate proc_macro;\n"));
    }
    undecided("#![no_std]\nextern crate proc_macro as pm;\n");
    undecided("#![no_std]\nextern crate proc_macro;\nuse proc_macro::*;\n");
    undecided("#![no_std]\nextern crate proc_macro;\nuse proc_macro::Span as S;\n");
}
