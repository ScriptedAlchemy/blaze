//! The view [`Semantics`], node by node on one fixture: every node of the file and of every
//! macro expansion in it is asked for its defs, types, resolutions, ranges and ancestors, and
//! the answers are held to the stored ones in `expect/`. Until the handle `Semantics` was
//! deleted these tests checked each answer against the handle's for the same node.

use base_db::EditionedFileId;
use blaze_alloc_count::assert_at_most;
use expect_test::expect_file;
use hir_expand::HirFileId;
use hir_expand::fixture;
use syntax::view::{self, AstNode as _, HasName as _};

use super::{HasSourceView, Semantics};
use crate::ModuleDef;

const FIXTURE: &str = r#"
//- /main.rs crate:main
macro_rules! id { ($($t:tt)*) => { $($t)* }; }
macro_rules! make_fn { ($name:ident) => { fn $name() -> u32 { 1 } }; }

mod inner {
    pub struct Point { pub x: u32, pub y: u32 }
    pub struct Pair(pub u32, pub i64);
    pub enum Shape { Dot(Point), Line { from: Point, to: Point }, Empty = 3 }
    pub union Bits { pub a: u32, pub b: f32 }
    pub trait Area { const SIDES: u32; type Out; fn area(&self) -> u32; }
    impl Area for Point {
        const SIDES: u32 = 0;
        type Out = u32;
        fn area(&self) -> u32 { self.x * self.y }
    }
    impl Point { pub fn new(x: u32) -> Self { Point { x, y: x } } }
}

use inner::{Area, Pair, Point, Shape};

const LIMIT: usize = 4;
static NAME: &str = "n";
type Alias<'a, T, const N: usize> = &'a [T; N];

make_fn!(made);

fn generic<'a, T: Copy, const N: usize>(xs: &'a [T; N]) -> Option<&'a T> { xs.first() }

fn origin() -> Point { Point::new(0) }

fn main() {
    id! { let p = origin(); }
    let q = Point { x: 1, y: 2 };
    let Point { x, y: why } = q;
    let pair = Pair(1, 2);
    let Pair(first, _) = pair;
    let _sum = p.area() + x + why + first + pair.0 as u32 + made();
    let _s = Shape::Dot(p);
    let _l = Shape::Line { from: q, to: q };
    let arr = [0u8; LIMIT];
    let _g = generic(&arr);
    let add = |a: u32, b: u32| a + b;
    let _r = 'outer: loop { break 'outer add(id!(1), 2); };
    let _n = NAME.len() + <Point as Area>::SIDES as usize;
    match _s { Shape::Dot(Point { x: px, .. }) => { let _ = px; } Shape::Line { .. } | Shape::Empty => {} }
}
"#;

/// The file's root and the roots of every macro expansion under it, nested ones included,
/// each with its file.
fn roots<'db>(
    vsema: &Semantics<'db>,
    file_id: EditionedFileId,
) -> Vec<(HirFileId, view::Node<'db>)> {
    let mut roots = vec![(HirFileId::from(file_id), vsema.parse(file_id).syntax())];
    let mut at = 0;
    while let Some(&(_, root)) = roots.get(at) {
        at += 1;
        for call in root.descendants().filter_map(view::MacroCall::cast) {
            if let Some(expansion) = vsema.expand_macro_call(call) {
                roots.push((expansion.file_id, expansion.value));
            }
        }
    }
    roots
}

/// One line of a stored answer: what was asked, of the node at `range` in `file`.
fn line(
    out: &mut String,
    file: HirFileId,
    range: syntax::TextRange,
    what: &str,
    answer: impl std::fmt::Debug,
) {
    use std::fmt::Write as _;
    writeln!(out, "{file:?} {range:?} {what}: {answer:?}").unwrap();
}

macro_rules! record_def {
    ($vsema:ident, $out:ident, $file:ident, $node:ident, $checked:ident: $($kind:ident),* $(,)?) => {$(
        if let Some(v) = view::$kind::cast($node) {
            line(&mut $out, $file, $node.text_range(), concat!("to_def ", stringify!($kind)), $vsema.to_def(v));
            $checked += 1;
        }
    )*};
}

#[test]
fn to_def_of_every_node() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        let mut checked = 0u32;
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                record_def!(vsema, out, file, node, checked:
                    Module, Struct, Enum, Union, Trait, Impl, TypeAlias, Const, Static, Fn,
                    RecordField, TupleField, Variant, TypeParam, LifetimeParam, ConstParam,
                    Macro, SelfParam, Label, Adt, IdentPat, MacroCall,
                );
            }
        }
        assert!(checked > 60, "the fixture has defs of every kind, {checked} checked");
        expect_file!["expect/to_def.txt"].assert_eq(&fixture::stable_ids(&out));
    });
}

#[test]
fn ancestors_ranges_and_scopes_of_every_node() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                let at = node.text_range();
                assert_eq!(vsema.hir_file_for(node), file, "the file of {at:?} in {file:?}");
                let ancestors: Vec<_> = vsema
                    .ancestors_with_macros(node)
                    .map(|it| (it.kind(), it.text_range()))
                    .collect();
                line(&mut out, file, at, "ancestors_with_macros", ancestors);
                line(&mut out, file, at, "original_range", vsema.original_range(node));
                line(&mut out, file, at, "original_range_opt", vsema.original_range_opt(node));
                line(
                    &mut out,
                    file,
                    at,
                    "scope",
                    vsema.scope(node).map(|it| (it.module(), it.krate(), it.file_id())),
                );
            }
        }
        expect_file!["expect/ancestors_ranges_scopes.txt"].assert_eq(&fixture::stable_ids(&out));
    });
}

#[test]
fn types_and_resolutions_of_every_node() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        let (mut exprs, mut paths) = (0u32, 0u32);
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                let at = node.text_range();
                let mut line = |what: &str, answer: &dyn std::fmt::Debug| {
                    line(&mut out, file, at, what, answer)
                };
                if let Some(v) = view::Expr::cast(node) {
                    line(
                        "type_of_expr",
                        &vsema.type_of_expr(v).map(|it| (it.original, it.adjusted)),
                    );
                    line(
                        "resolve_expr_as_callable",
                        &vsema.resolve_expr_as_callable(v).map(|it| it.return_type()),
                    );
                    exprs += 1;
                }
                if let Some(v) = view::Pat::cast(node) {
                    line("type_of_pat", &vsema.type_of_pat(v).map(|it| (it.original, it.adjusted)));
                }
                if let Some(v) = view::IdentPat::cast(node) {
                    line("type_of_binding_in_pat", &vsema.type_of_binding_in_pat(v));
                    line("resolve_bind_pat_to_const", &vsema.resolve_bind_pat_to_const(v));
                }
                if let Some(v) = view::Path::cast(node) {
                    line("resolve_path", &vsema.resolve_path(v));
                    let per_ns = vsema
                        .resolve_path_per_ns(v)
                        .map(|it| (it.type_ns, it.value_ns, it.macro_ns));
                    line("resolve_path_per_ns", &per_ns);
                    let scope = vsema.scope(v.syntax()).expect("a path has a scope");
                    line("speculative_resolve_view", &scope.speculative_resolve_view(v));
                    paths += 1;
                }
                if let Some(v) = view::Type::cast(node) {
                    line("resolve_type", &vsema.resolve_type(v));
                }
                if let Some(v) = view::MethodCallExpr::cast(node) {
                    line("resolve_method_call", &vsema.resolve_method_call(v));
                    line(
                        "resolve_method_call_as_callable",
                        &vsema.resolve_method_call_as_callable(v).map(|it| it.return_type()),
                    );
                }
                if let Some(v) = view::FieldExpr::cast(node) {
                    line("resolve_field", &vsema.resolve_field(v));
                }
                if let Some(v) = view::RecordExprField::cast(node) {
                    line("resolve_record_field", &vsema.resolve_record_field(v));
                }
                if let Some(v) = view::RecordPatField::cast(node) {
                    line("resolve_record_pat_field", &vsema.resolve_record_pat_field(v));
                }
                if let Some(v) = view::TupleStructPat::cast(node) {
                    line(
                        "resolve_tuple_struct_pat_fields",
                        &vsema.resolve_tuple_struct_pat_fields(v),
                    );
                }
                if let Some(v) = view::RecordExpr::cast(node) {
                    line("resolve_variant", &vsema.resolve_variant(v));
                }
                if let Some(v) = view::MacroCall::cast(node) {
                    line("resolve_macro_call", &vsema.resolve_macro_call(v));
                    line("resolve_macro_call_arm", &vsema.resolve_macro_call_arm(v));
                }
                if let Some(v) = view::Lifetime::cast(node) {
                    line("resolve_lifetime_param", &vsema.resolve_lifetime_param(v));
                    line("resolve_label", &vsema.resolve_label(v));
                }
                if let Some(v) = view::Item::cast(node) {
                    line("resolve_attr_macro_call", &vsema.resolve_attr_macro_call(v));
                }
            }
        }
        assert!(exprs > 50 && paths > 30, "{exprs} expressions, {paths} paths");
        expect_file!["expect/types_resolutions.txt"].assert_eq(&fixture::stable_ids(&out));
    });
}

#[test]
fn descending_every_token() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        let mut into_macros = 0u32;
        for token in
            vsema.parse(file_id).syntax().descendants_with_tokens().filter_map(|it| it.into_token())
        {
            if token.text_range().is_empty() {
                continue;
            }
            let views: Vec<_> = vsema
                .descend_into_macros_exact(token)
                .into_iter()
                .map(|it| (it.kind(), it.text_range(), vsema.hir_file_for(it.parent().unwrap())))
                .collect();
            into_macros += u32::from(views.iter().any(|(.., file)| file.is_macro()));
            let what = format!("descend_into_macros_exact {:?}", token.text());
            line(&mut out, file_id.into(), token.text_range(), &what, views);
        }
        assert!(into_macros >= 3, "the tokens of `id!` and `make_fn!` descend, {into_macros} did");
        expect_file!["expect/descend.txt"].assert_eq(&fixture::stable_ids(&out));
    });
}

#[test]
fn source_view_of_every_def() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        use std::fmt::Write as _;
        let vsema = Semantics::new(&db);
        let krate = vsema.file_to_module_def(file_id.file_id(&db)).expect("a module").krate(&db);
        let mut out = String::new();
        let mut checked = 0u32;
        for module in krate.modules(&db) {
            let def = module.definition_source_view(&db);
            writeln!(
                out,
                "{module:?} definition_source_view: {:?}",
                (def.file_id, def.value.node().kind(), def.value.node().text_range()),
            )
            .unwrap();
            writeln!(
                out,
                "{module:?} declaration_source_view: {:?}",
                module
                    .declaration_source_view(&db)
                    .map(|it| (it.file_id, it.value.syntax().text_range())),
            )
            .unwrap();
            for decl in module.declarations(&db) {
                macro_rules! compare {
                    ($def:expr) => {{
                        let def = $def;
                        writeln!(
                            out,
                            "{def:?} source_view: {:?}",
                            def.source_view(&db).map(|it| (
                                it.file_id,
                                it.value.syntax().kind(),
                                it.value.syntax().text_range()
                            )),
                        )
                        .unwrap();
                        checked += 1;
                    }};
                }
                match decl {
                    ModuleDef::Function(it) => compare!(it),
                    ModuleDef::Adt(it) => {
                        compare!(it);
                        for variant in it.as_enum().map(|it| it.variants(&db)).unwrap_or_default() {
                            compare!(variant);
                            for field in variant.fields(&db) {
                                compare!(field);
                            }
                        }
                        if let crate::Adt::Struct(s) = it {
                            for field in s.fields(&db) {
                                compare!(field);
                            }
                        }
                    }
                    ModuleDef::Const(it) => compare!(it),
                    ModuleDef::Static(it) => compare!(it),
                    ModuleDef::Trait(it) => compare!(it),
                    ModuleDef::TypeAlias(it) => compare!(it),
                    ModuleDef::Macro(it) => compare!(it),
                    _ => {}
                }
            }
            for impl_ in module.impl_defs(&db) {
                writeln!(
                    out,
                    "{impl_:?} source_view: {:?}",
                    impl_.source_view(&db).map(|it| (it.file_id, it.value.syntax().text_range())),
                )
                .unwrap();
                checked += 1;
            }
        }
        assert!(checked > 20, "{checked} defs checked");
        expect_file!["expect/source_view.txt"].assert_eq(&fixture::stable_ids(&out));
    });
}

#[test]
fn a_file_only_a_block_declares_is_a_module_whose_paths_resolve() {
    // No crate def map lists `m.rs`: only `main`'s block's def map has the module `m`.
    let fx = fixture::db(
        r#"
//- /main.rs crate:main
pub fn d() {}
fn main() {
    #[path = "m.rs"]
    mod m;
}
//- /m.rs
pub fn inner() {
    crate::d();
}
"#,
    );
    let m = *fx.files().last().expect("m.rs");
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let vsema = Semantics::new(&db);
        assert!(
            vsema.file_to_module_def(m.file_id(&db)).is_some(),
            "m.rs is the module `m` of `main`'s block"
        );
        assert!(
            vsema.find_parent_file(HirFileId::from(m)).is_some(),
            "the `mod m;` declaring m.rs"
        );
        let root = vsema.parse(m).syntax();
        let path = root
            .descendants()
            .filter_map(view::Path::cast)
            .find(|it| it.qualifier().is_some())
            .expect("`crate::d`");
        assert!(
            matches!(
                vsema.resolve_path(path),
                Some(crate::PathResolution::Def(ModuleDef::Function(_)))
            ),
            "`crate::d` in m.rs resolves to the function `d`"
        );
    });
}

#[test]
fn a_path_module_in_another_packages_source_root_is_a_module_of_the_crate_it_is_checked_in() {
    // `m.rs` is in `b`'s source root, and only `a` declares it (`#[path]` reaching out of `a`'s
    // directory): a check of `a` maps its syntax to `a`'s module `m`, as rustc compiles it.
    let fx = fixture::db(
        r#"
//- /a/src/main.rs crate:a
#[path = "../../b/tests/m.rs"]
mod m;
pub fn d() {}
fn main() {}
//- /b/src/lib.rs crate:b new_source_root:local
pub fn b() {}
//- /b/tests/m.rs
pub fn inner() {
    crate::d();
}
"#,
    );
    let main = fx.file_at("/a/src/main.rs");
    let m = fx.file_at("/b/tests/m.rs");
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let a = Semantics::new(&db)
            .file_to_module_def(main.file_id(&db))
            .expect("main.rs is `a`'s root")
            .krate(&db);
        let within =
            std::sync::Arc::new(blaze_hash::HashSet::from_iter_in([a.base()], std::alloc::Global));
        let vsema = Semantics::new_within(&db, within);
        let module =
            vsema.file_to_module_def(m.file_id(&db)).expect("m.rs is the module `m` of `a`");
        assert_eq!(module.krate(&db), a);
        let root = vsema.parse(m).syntax();
        let path = root
            .descendants()
            .filter_map(view::Path::cast)
            .find(|it| it.qualifier().is_some())
            .expect("`crate::d`");
        assert!(vsema.scope(path.syntax()).is_some(), "`crate::d` in m.rs has a scope");
        assert!(
            matches!(
                vsema.resolve_path(path),
                Some(crate::PathResolution::Def(ModuleDef::Function(_)))
            ),
            "`crate::d` in m.rs resolves to `a`'s function `d`"
        );
    });
}

const CRATE_CONTEXT_FIXTURE: &str = r#"
//- /src/lib.rs crate:production
mod shared;
//- /src/shared.rs
pub struct Common;
#[cfg(test)]
pub struct TestOnly;
pub mod nested {
    pub fn common() { let _: super::Common; }
    #[cfg(test)]
    pub fn test_only() { let _: super::TestOnly; }
}
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!("included.rs");
//- /src/included.rs
pub mod generated {
    pub fn common() { let _: super::Common; }
}
//- /tests/lib.rs crate:test:unit cfg:test new_source_root:local
#[path = "../src/shared.rs"]
mod shared;
"#;

/// Each sequence uses a fresh database, then drops and recreates real `Semantics` without
/// changing its revision. `None` is unscoped (production); `Some` prefers production or test.
fn check_warm_crate_contexts(
    file: &str,
    check: impl Fn(&Semantics<'_>, EditionedFileId, crate::Crate, bool),
) {
    for contexts in [
        &[Some(true), Some(true)][..], // Cold test control, then the same preference again.
        &[None, None, Some(true), Some(true), Some(false), Some(false), Some(true), None, None][..],
    ] {
        let fx = fixture::db(CRATE_CONTEXT_FIXTURE);
        let file = fx.file_at(file);
        let production_root = fx.file_at("/src/lib.rs");
        let test_root = fx.file_at("/tests/lib.rs");
        let db = fx.into_db();
        blaze_query::db::attach(&db, || {
            let crates = base_db::all_crates(&db);
            let krate = |root: EditionedFileId| -> crate::Crate {
                crates
                    .iter()
                    .copied()
                    .find(|it| it.data(&db).root_file_id == root.file_id(&db))
                    .expect("the fixture's crate root")
                    .into()
            };
            let (production, test) = (krate(production_root), krate(test_root));
            let revision = db.nonce_and_revision();
            for &context in contexts {
                let is_test = context == Some(true);
                let expected = if is_test { test } else { production };
                let vsema = match context {
                    None => Semantics::new(&db),
                    Some(_) => {
                        let crates = blaze_hash::HashSet::from_iter_in(
                            [expected.base()],
                            std::alloc::Global,
                        );
                        let within = std::sync::Arc::new(crates); // alloc: this test instance's preferred crates
                        Semantics::new_within(&db, within)
                    }
                };
                // Positive warm control within each instance as well as across instances.
                for _ in 0..2 {
                    check(&vsema, file, expected, is_test);
                }
                drop(vsema);
                assert_eq!(db.nonce_and_revision(), revision, "no input changed between contexts");
            }
        });
    }
}

#[test]
fn warm_file_resolution_tracks_the_preferred_crate() {
    check_warm_crate_contexts("/src/shared.rs", |vsema, file, expected, is_test| {
        let db = vsema.db;
        let root = vsema.parse(file).syntax();
        let module = vsema.file_to_module_def(file.file_id(db)).expect("shared.rs is a module");
        assert_eq!(module.krate(db), expected, "the file belongs to the current context");
        let strukt = |name| {
            root.descendants()
                .filter_map(view::Struct::cast)
                .find(|it| it.name().is_some_and(|it| it.text() == name))
                .expect("the shared file's struct")
        };
        let common = vsema.to_def(strukt("Common")).expect("Common is in both contexts");
        assert_eq!(common.module(db), module);
        let nested = root.descendants().find_map(view::Module::cast).expect("mod nested");
        let nested = vsema.to_def(nested).expect("nested is in both contexts");
        assert_eq!(nested.parent(db), Some(module));
        let path = |text| {
            root.descendants()
                .filter_map(view::Path::cast)
                .find(|it| it.syntax().text() == text)
                .expect("the nested function's path")
        };
        let common_path = path("super::Common");
        assert_eq!(vsema.scope(common_path.syntax()).expect("a common scope").module(), nested);
        let resolution = |it| crate::PathResolution::Def(ModuleDef::Adt(crate::Adt::Struct(it)));
        assert_eq!(vsema.resolve_path(common_path), Some(resolution(common)));
        let test_only = vsema.to_def(strukt("TestOnly"));
        assert_eq!(test_only.is_some(), is_test, "TestOnly exists only with cfg(test)");
        if let Some(test_only) = test_only {
            assert_eq!(test_only.module(db), module);
        }
        assert_eq!(vsema.resolve_path(path("super::TestOnly")), test_only.map(resolution));
    });
}

#[test]
fn warm_include_resolution_tracks_the_preferred_crate() {
    check_warm_crate_contexts("/src/included.rs", |vsema, file, expected, _| {
        let db = vsema.db;
        let root = vsema.parse(file).syntax();
        let token = root
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.text() == "Common")
            .expect("the included super::Common token");
        // Query the include mapping before any file-to-module lookup can populate it.
        let mapped = vsema.descend_into_macros_exact(token);
        assert!(!mapped.is_empty(), "the include token maps into its expansion");
        for token in mapped {
            let parent = token.parent().expect("a mapped token has a parent");
            assert!(vsema.hir_file_for(parent).is_macro(), "the token is in an include expansion");
            let path = parent
                .ancestors()
                .filter_map(view::Path::cast)
                .find(|it| it.qualifier().is_some())
                .expect("the mapped parent path");
            let scope = vsema.scope(path.syntax()).expect("the included function has a scope");
            assert_eq!(scope.krate(), expected, "the include belongs to the current context");
            let Some(crate::PathResolution::Def(ModuleDef::Adt(crate::Adt::Struct(common)))) =
                vsema.resolve_path(path)
            else {
                panic!("the included super::Common resolves to a struct");
            };
            assert_eq!(common.module(db).krate(db), expected);
            assert_eq!(scope.module().parent(db), Some(common.module(db)));
        }
    });
}

#[test]
fn a_warm_lookup_walk_allocates_nothing() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let vsema = Semantics::new(&db);
        let root = vsema.parse(file_id).syntax();
        let strukt = root.descendants().find_map(view::Struct::cast).expect("`struct Point`");
        let field = root.descendants().find_map(view::RecordField::cast).expect("field `x`");
        let walk = || {
            let s = vsema.to_def(strukt);
            let f = vsema.to_def(field);
            let file = vsema.hir_file_for(field.syntax());
            let depth = vsema.ancestors_with_macros(field.syntax()).count();
            (s, f, file, depth)
        };
        let warm = walk();
        let measured = assert_at_most(0, "to_def, hir_file_for and ancestors_with_macros", walk);
        assert_eq!(warm, measured);
        assert!(warm.0.is_some() && warm.1.is_some());
    });
}
