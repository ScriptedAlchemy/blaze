use syntax::{TextRange, TextSize};

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: borrowck
//
// An error of rustc's borrow checker: a use of a moved or uninitialized value, a mutation of
// what is not mutable, a borrow that conflicts with another or does not live long enough. Also
// rustc's `unused_mut` lint, which its borrow checker gives: a variable declared `mut` that is
// never mutated.
pub(crate) fn borrowck_error(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckError,
) -> Diagnostic {
    // rustc gives some errors no code, as "lifetime may not live long enough".
    let code = match d.code {
        "" => DiagnosticCode::Ra("borrowck", crate::Severity::Error),
        "unused_mut" => DiagnosticCode::RustcLint("unused_mut"),
        code => DiagnosticCode::RustcHardError(code),
    };
    let mut diagnostic = Diagnostic::new_with_syntax_node_ptr(ctx, code, d.message.clone(), d.node);
    // rustc's span of an expression a macro writes, `&$e`, is in the macro's definition, where
    // its first token is; one the caller writes is the caller's. A definition in another file
    // stays at the call.
    if let Some(file) = d.node.file_id.macro_file() {
        let start = d.node.value.text_range().start();
        let (origin, context) = hir::InMacroFile::new(file, start).original_file_range(ctx.sema.db);
        if !context.is_root() && origin.file_id.file_id(ctx.sema.db) == diagnostic.range.file_id {
            diagnostic.range.range = origin.range;
        }
    }
    if d.at_end {
        let range = diagnostic.range.range;
        if let Some(last) = range.end().checked_sub(TextSize::new(1))
            && last >= range.start()
        {
            diagnostic.range.range = TextRange::new(last, range.end());
        }
    }
    diagnostic.stable()
}

// Diagnostic: borrowck-skipped
//
// The borrow checker did not check a body, because MIR lowering does not support something
// in it. Reported only for the borrow checker's test harness.
pub(crate) fn borrowck_skipped(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckSkipped,
) -> Diagnostic {
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::Ra("borrowck-skipped", crate::Severity::WeakWarning),
        format!("not borrow checked: {}", d.reason),
        d.node,
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn impl_trait_captures_elided_lifetime_2024() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2024
struct S(i32);
impl S {
    fn get(&self) -> impl Sized { &self.0 }
    fn kept(&self) -> impl Sized + use<> {}
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let kept = s.kept();
    s.set();
    consume(kept);
}
"#,
        );
    }

    #[test]
    fn impl_trait_captures_elided_lifetime_2021() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2021
struct S(i32);
impl S {
    fn get(&self) -> impl Sized + '_ { &self.0 }
    fn copied(&self) -> impl Sized { self.0 }
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let copied = s.copied();
    s.set();
    consume(copied);
}
"#,
        );
    }

    #[test]
    fn constant_of_a_signature_is_borrow_checked() {
        check_diagnostics(
            r#"
//- minicore: sized
pub struct X<const N: usize = {
    let n: usize;
    n
  //^ error: used binding `n` isn't initialized
}>;
pub fn f() -> [u8; { let n = 1; n }] {
    [0]
}
"#,
        );
    }

    #[test]
    fn higher_ranked_bound_holds_for_any_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, fn
trait Deserialize<'de> {}
trait DeserializeOwned: for<'de> Deserialize<'de> {}
impl<T> DeserializeOwned for T where T: for<'de> Deserialize<'de> {}
impl<'de: 'a, 'a> Deserialize<'de> for &'a str {}
impl<'de> Deserialize<'de> for u32 {}
fn owned<T: DeserializeOwned>() {}
trait Write {}
fn handler(_: &u32, _: &mut dyn Write) {}
fn run(_: impl FnMut(&u32, &mut dyn Write)) {}
fn main() {
    owned::<u32>();
    owned::<&'static str>();
  //^^^^^^^^^^^^^^^^^^^^^^^ error: higher-ranked lifetime error
    run(handler);
}
"#,
        );
    }

    #[test]
    fn one_fn_pointer_of_items_or_closures_holds_for_any_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, fn, copy
fn up(_: &u32, _: &u32) -> bool { true }
fn down(_: &u32, _: &u32) -> bool { false }
fn sort(_: impl FnMut(&u32, &u32) -> bool) {}
fn pick(reverse: bool) {
    sort(if reverse { up } else { down });
    sort(match reverse {
        true => |_: &u32, _: &u32| true,
        false => |_: &u32, _: &u32| false,
    });
}
struct Line<'a>(&'a u32);
fn mode<'a>(_: &mut Line<'a>) {}
fn name<'a>(_: &mut Line<'a>) {}
fn element<'a>(first: bool) -> impl Fn(&mut Line<'a>) {
    match first {
        true => mode,
        false => name,
    }
}
"#,
        );
    }

    #[test]
    fn destructuring_assignment_moves_what_it_takes() {
        check_diagnostics(
            r#"
//- minicore: copy
struct NC;
struct S { a: u8, b: NC }
union U { field: u8 }
fn copies(r: &S, u: &U, mut a: u8, mut field: u8) {
    S { a, .. } = *r;
    S { a: _, .. } = *r;
    unsafe { U { field } = *u };
    let _ = (a, field);
}
fn moves(r: &S, mut b: NC) {
    S { b, .. } = *r;
                //^^ error: cannot move out of `r.b` which is behind a shared reference
    let _ = b;
}
"#,
        );
    }

    #[test]
    fn a_body_with_an_error_is_not_borrow_checked() {
        check_diagnostics(
            r#"
//- minicore: copy
struct NC;
fn tainted(x: NC) -> u32 {
    let _y = x;
    let _z = x;
    NC as u32
  //^^^^^^^^^ error: non-primitive cast: `NC` as `u32`
}
"#,
        );
    }

    // minicore's `impl<T> Default for &[T]`.
    #[test]
    fn an_empty_array_is_promoted_whatever_its_elements() {
        check_diagnostics(
            r#"
//- minicore: copy, cell, unsize, coerce_unsized
use core::cell::Cell;
fn shared<T>() -> &'static [T] { &[] }
fn unique<'a, T>() -> &'a mut [T] { &mut [] }
fn cells() -> &'static [Cell<u8>] { &[] }
fn one<T>(x: T) -> &'static [T] { &[x] }
                                //^^^^ error: returns a value referencing data owned by the current function
"#,
        );
    }

    // rustc's tests/ui/expr/if/expr-stack-overflow.rs: each expression's syntax was found by
    // walking down from the body's root, quadratic in the chain, and lowering it recursed
    // past the stack.
    #[test]
    fn a_5204_arm_else_if_chain_is_diagnosed_in_linear_time() {
        let mut fixture = String::from(
            "//- minicore: copy\npub fn banana(v: u32) -> u32 {\n    if v == 0 { 0 }\n",
        );
        for arm in 1..5204 {
            fixture.push_str(&format!("    else if v == {arm} {{ {arm} }}\n"));
        }
        fixture.push_str("    else { 5204 }\n}\n");
        let started = std::time::Instant::now();
        check_diagnostics(&fixture);
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(60), "took {took:?}");
    }

    // rustc's tests/ui/associated-type-bounds/issue-83017.rs.
    #[test]
    fn bound_of_an_associated_type_in_a_for_binds_the_outer_lifetimes() {
        check_diagnostics(
            r#"
//- minicore: sized
trait TraitA<'a> { type AsA; }
trait TraitB<'a, 'b> { type AsB; }
trait TraitC<'a, 'b, 'c> {}
struct X;
impl<'a, 'b, 'c> TraitC<'a, 'b, 'c> for X {}
struct Y;
impl<'a, 'b> TraitB<'a, 'b> for Y { type AsB = X; }
struct Z;
impl<'a> TraitA<'a> for Z { type AsA = Y; }
fn foo<T>()
where
    for<'a> T: TraitA<'a, AsA: for<'b> TraitB<'a, 'b, AsB: for<'c> TraitC<'a, 'b, 'c>>>,
{
}
fn main() {
    foo::<Z>();
}
"#,
        );
    }

    #[test]
    fn borrowed_object_default_keeps_its_reference_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized
trait Trait {}
impl Trait for i32 {}
struct Holder<'a> { value: &'a (dyn Trait + 'a) }
fn identity(x: &dyn Trait) -> &dyn Trait { x }
fn wrap(x: &dyn Trait) -> Holder<'_> { Holder { value: x } }
fn distinct<'a, 'b>(x: &'a dyn Trait, _: &'b dyn Trait) -> &'a dyn Trait { x }
"#,
        );
    }

    #[test]
    fn borrowed_objects_keep_independent_and_static_requirements() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized
trait Trait {}
impl Trait for i32 {}
struct Holder<'a> { value: &'a (dyn Trait + 'a) }
fn wrap(x: &dyn Trait) -> Holder<'_> { Holder { value: x } }
fn wrong<'a, 'b>(_: &'a dyn Trait, y: &'b dyn Trait) -> &'a dyn Trait {
    y
  //^ error: lifetime may not live long enough
}
fn static_object(x: &dyn Trait) -> &(dyn Trait + 'static) {
    x
  //^ error: lifetime may not live long enough
}
fn local<'a>() -> Holder<'a> {
    let value = 1;
    Holder { value: &value }
  //^^^^^^^^^^^^^^^^^^^^^^^^ error: cannot return reference to local variable
}
fn local_call<'a>() -> Holder<'a> {
    let value = 1;
    wrap(&value)
  //^^^^^^^^^^^^ error: cannot return reference to local variable
}
"#,
        );
    }

    // rustc's tests/ui/dyn-compatibility/multiple-supers-should-work.rs.
    #[test]
    fn a_trait_object_keeps_projections_that_become_the_same() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait Sup<T> { type Assoc; }
impl<T> Sup<T> for () { type Assoc = T; }
trait Trait<A, B>: Sup<A, Assoc = A> + Sup<B, Assoc = B> {}
impl<T, U> Trait<T, U> for () {}
fn main() {
    let x: &dyn Trait<(), _> = &();
    let _y: &dyn Trait<_, ()> = x;
}
"#,
        );
    }

    // rustc's tests/ui/traits/trait-upcasting/higher-ranked-upcasting-ok.rs.
    #[test]
    fn upcasting_a_higher_ranked_trait_object() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait Supertrait<'a, 'b> {}
trait Subtrait<'a, 'b>: Supertrait<'a, 'b> {}
impl Supertrait<'_, '_> for () {}
impl Subtrait<'_, '_> for () {}
fn ok(x: &dyn for<'a, 'b> Subtrait<'a, 'b>) -> &dyn for<'a> Supertrait<'a, 'a> {
    x
}
fn main() {
    ok(&());
}
"#,
        );
    }

    #[test]
    fn unit_expression_initializes_its_place() {
        check_diagnostics(
            r#"
//- minicore: copy
#![deny(unused_mut)]
fn f() -> () {
    let mut x: i32;
    let z: () = x = 10;
    let w = z;
    let mut q: () = x = 11;
    q = x += 1;
    let mut b: () = { x += 1; };
    b = ();
    let mut l: () = loop { break; };
    l = ();
    let mut i: () = if x > 0 { x = 1 };
    i = ();
    let _ = (q, b, l, i, x);
    w
}
"#,
        );
    }

    // hashlink's `Keys`, hashbrown's `set::Iter`: `K: 'a` is rustc's `inferred_outlives_of` the
    // type however many fields down its `&'a K` is.
    #[test]
    fn implied_outlives_of_nested_fields() {
        check_diagnostics(
            r#"
//- minicore: sized, copy, phantom_data, option
use core::marker::PhantomData;
pub struct Iter<'a, K, V> { marker: PhantomData<(&'a K, &'a V)> }
impl<'a, K, V> Iter<'a, K, V> {
    fn again(&self) -> Self { Iter { marker: PhantomData } }
    fn key(&self) -> Option<&'a K> { None }
}
pub struct Keys<'a, K, V> { inner: Iter<'a, K, V> }
impl<'a, K, V> Keys<'a, K, V> {
    fn again(&self) -> Keys<'a, K, V> { Keys { inner: self.inner.again() } }
    fn key(&self) -> Option<&'a K> { self.inner.key() }
}
pub struct Set<'a, K> { iter: Keys<'a, K, ()> }
impl<K> Set<'_, K> {
    fn again(&self) -> Self { Set { iter: self.iter.again() } }
}
// A field that does not need `K: 'a` implies nothing of it.
pub struct NoRef<'a, K> { marker: PhantomData<&'a ()>, k: K }
fn nothing_implied<'a, K>(_: NoRef<'a, K>) {
    let _ = Iter::<'a, K, ()> { marker: PhantomData };
          //^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ error: the parameter type may not live long enough
}
"#,
        );
    }

    // plotters' `MeshStyle` and zerocopy's `ProjectField`: an implied bound on an associated type
    // is one on what it normalizes to, where the body asks it of that.
    #[test]
    fn implied_outlives_of_an_associated_type_normalize() {
        check_diagnostics(
            r#"
//- minicore: sized, fn, option
trait Ranged { type Value; }
struct Style<'b, X: Ranged> { format: Option<&'b dyn Fn(&X::Value)> }
impl<'b, XT, X: Ranged<Value = XT>> Style<'b, X> {
    fn again(&self) -> Self { Style { format: None } }
}
trait Tagged { type Tag; }
struct Ptr<'a, T>(&'a T);
struct Wrap<X>(X);
impl<X: Tagged> Tagged for Wrap<X> { type Tag = X::Tag; }
trait Project: Tagged { fn go<'a>(ptr: Ptr<'a, Self::Tag>); }
impl<X: Project> Project for Wrap<X> {
    fn go<'a>(ptr: Ptr<'a, <Self as Tagged>::Tag>) { X::go(ptr) }
}
struct Unrelated<'b, X>(&'b (), X);
fn nothing_implied<'b, XT, X: Ranged<Value = XT>>(_: Unrelated<'b, X>) {
    let _ = Style::<'b, X> { format: None };
          //^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ error: the parameter type may not live long enough
}
"#,
        );
    }

    // smallvec's and allocator-api2's `Index`: `<[T] as Index<I>>::Output` is the
    // `<I as SliceIndex<[T]>>::Output` the signature implies outlives its reference.
    #[test]
    fn associated_type_normalizes_to_the_one_the_signature_knows() {
        check_diagnostics(
            r#"
//- minicore: sized, index
use core::ops::Index;
trait SliceIndex<T> { type Output: ?Sized; }
struct Slice<T>(T);
impl<T, I: SliceIndex<T>> Index<I> for Slice<T> {
    type Output = I::Output;
    fn index(&self, _: I) -> &I::Output { loop {} }
}
struct Vec2<T>(Slice<T>);
impl<T, I: SliceIndex<T>> Index<I> for Vec2<T> {
    type Output = I::Output;
    fn index(&self, index: I) -> &Self::Output { Index::index(&self.0, index) }
}
fn unrelated<'a, 'b, T, I: SliceIndex<T>>(s: &'a Slice<T>, i: I, _: &'b ()) -> &'b I::Output {
    Index::index(s, i)
  //^^^^^^^^^^^^^^^^^^ error: lifetime may not live long enough
}
"#,
        );
    }

    // indexmap's, boxcar's and rayon's `[].iter_mut()`: an autoref is `&mut []` as written.
    #[test]
    fn an_autoref_of_an_empty_array_is_promoted() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, slice
struct IterMut<'a, T>(&'a mut [T]);
trait AsIter<T> { fn walk(&mut self) -> IterMut<'_, T>; }
impl<T> AsIter<T> for [T] {
    fn walk(&mut self) -> IterMut<'_, T> { IterMut(self) }
}
fn empty<'a, T>() -> IterMut<'a, T> { [].walk() }
fn one<'a, T>(x: T) -> IterMut<'a, T> { [x].walk() }
                                      //^^^^^^^^^^ error: returns a value referencing data owned by the current function
"#,
        );
    }

    // tracing-core's `impl Value for dyn Error`: `Self` is the object the impl's header writes,
    // `'static` by default, where a `&dyn Trait` the signature writes has the reference's.
    #[test]
    fn self_of_an_impl_for_an_object_keeps_its_static_default() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized
trait Err {}
trait Visit { fn error(&mut self, it: &(dyn Err + 'static)); }
trait Value {
    fn record(&self, visitor: &mut dyn Visit);
    fn get(&mut self) -> &mut Self;
    fn other<'s>(&'s self, o: &dyn Err) -> &'s (dyn Err + 's);
    fn stat(&self) -> &'static (dyn Err + 'static);
}
impl Value for dyn Err {
    fn record(&self, visitor: &mut dyn Visit) { visitor.error(self) }
    fn get(&mut self) -> &mut Self { self }
    fn other<'s>(&'s self, o: &dyn Err) -> &'s (dyn Err + 's) {
        o
      //^ error: lifetime may not live long enough
    }
    fn stat(&self) -> &'static (dyn Err + 'static) {
        self
      //^^^^ error: lifetime may not live long enough
    }
}
impl dyn Err {
    fn inherent(&mut self) -> &mut Self { self }
    fn outer(&mut self) -> &mut Self { self.inherent() }
}
fn keep<'a>(x: &'a mut (dyn Err + 'static)) -> &'a mut (dyn Err + 'static) { x.get().get() }
"#,
        );
    }

    // rowan's `parent_ancestors` in syntax's `ancestors_at_offset`: an edition 2021 `impl Trait`
    // does not capture the lifetime the signature elides, so what it returns may outlive it.
    #[test]
    fn impl_trait_of_2021_returned_past_the_borrow_it_does_not_capture() {
        check_diagnostics(
            r#"
//- minicore: sized, copy, fn
//- /main.rs edition:2021
struct Tok;
impl Tok {
    fn up(&self) -> impl Sized { 1u32 }
    fn kept(&self) -> impl Sized + '_ { self }
}
fn free(t: &Tok) -> impl Sized { let _ = t; 1u32 }
fn direct() -> impl Sized { let t = Tok; t.up() }
fn through() -> impl Sized { let t = Tok; let u = t.up(); u }
fn tuple() -> (impl Sized, u32) { let t = Tok; (t.up(), 1) }
fn closure() { let f = |t: Tok| t.up(); let _ = f(Tok); }
fn of_free() { let f = |t: Tok| free(&t); let _ = f(Tok); }
fn captured() -> impl Sized { let t = Tok; t.kept() }
                                         //^^^^^^^^ error: cannot return reference to local variable
"#,
        );
    }

    #[test]
    fn unused_mut_self() {
        check_diagnostics(
            r#"
#![deny(unused_mut)]
struct S(u8);
impl S {
    fn read(mut self) -> u8 { self.0 }
          //^^^^^^^^ error: variable does not need to be mutable
    fn write(mut self) -> u8 { self.0 = 1; self.0 }
}
"#,
        );
    }
}
