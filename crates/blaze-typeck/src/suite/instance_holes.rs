//! A one-shot instance question (instance.rs `select_impl`, `normalize_projection`,
//! `resolve_instance`; drop_impl.rs `adt_drop_impl`) read through a hole answers nothing.
//!
//! `Reads` turns a world's `Missing` into a hole whose stand-in is an empty answer, so a body's
//! run goes on and names every need at once (world.rs `Hole`); a body's run that met one makes
//! no result (lib.rs `check`). The instance questions had no such rule: the stand-in decided
//! them. At tempo's `CharPredicateSearcher<'a, F>` read in a generic body, the impls of
//! `MultiCharEq` at `IsWhitespace` were never asked while checking, the empty stand-in made
//! `impl<C: MultiCharEq> Pattern for MultiCharEqPattern<C>` not hold, and the GAT
//! `<MultiCharEqPattern<IsWhitespace> as Pattern>::Searcher<'a>` was refused as a mismatch
//! (census classes 37 and 35). Each case holds back one question and wants it named; its
//! control answers the question and wants the plain answer.

use blaze_own::{Alloc, Vec};
use std::alloc::Global;

use crate::ids::{ItemKey, List, Name};
use crate::instance::ResolveError;
use crate::portable::{PNode, PortableTys};
use crate::toy::{FieldTy, ToyStruct, ToyWorld, struct_key};
use crate::world::{Question, Shape};

/// A trait no record of the toy world names: only its impls are asked.
const TRAIT: ItemKey = ItemKey(0x7_0000);

fn world(held: Option<Question>) -> ToyWorld {
    let mut fields = Vec::new_in(Global);
    fields.push((Name(1), FieldTy::I32));
    let mut structs = Vec::new_in(Global);
    structs.push(ToyStruct { fields });
    let mut w = match ToyWorld::new(&structs, &[FieldTy::Struct(0)], FieldTy::I32) {
        Ok(w) => w,
        Err(e) => panic!("the toy world could not be built: {e}"),
    };
    w.missing.extend(held);
    w
}

fn select(w: &ToyWorld) -> Result<Option<ItemKey>, ResolveError> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let s0 = ground.intern(PNode::Adt(struct_key(0), List::EMPTY)).expect("the ground table has room");
    crate::instance::select_impl(w, TRAIT, &[s0], &mut ground, Alloc::global()).map(|o| o.map(|(imp, _)| imp))
}

/// `S0: Trait` with the impls of `Trait` at `S0` held back: `Missing` naming that question.
/// Without `instance.rs` `unholed` it is `Ok(None)`, "no impl holds", read from the stand-in.
/// Prints `instance holes: select Err(Missing(ImplsOf(..)))`.
#[test]
fn a_selection_through_an_unmade_bucket_names_the_bucket() {
    let bucket = Question::ImplsOf(TRAIT, Shape::Adt(struct_key(0)));
    let got = select(&world(Some(bucket)));
    std::println!("instance holes: select {got:?}");
    assert_eq!(got, Err(ResolveError::Missing(bucket)));
    // Control: the world answers the bucket (no impl in it), so no impl holds.
    assert_eq!(select(&world(None)), Ok(None));
}

/// `S0`'s `Drop` impl with the bucket held back: `Missing`, never "no `Drop` impl" (which
/// would drop an `S0` by its fields alone). Prints `instance holes: drop Err(Missing(..))`.
#[test]
fn a_drop_impl_through_an_unmade_bucket_names_the_bucket() {
    let bucket = Question::ImplsOf(TRAIT, Shape::Adt(struct_key(0)));
    let got = crate::drop_impl::adt_drop_impl(&world(Some(bucket)), TRAIT, struct_key(0), Alloc::global());
    std::println!("instance holes: drop {got:?}");
    assert_eq!(got, Err(ResolveError::Missing(bucket)));
    // Control: the bucket answered, empty: no `Drop` impl.
    assert_eq!(crate::drop_impl::adt_drop_impl(&world(None), TRAIT, struct_key(0), Alloc::global()), Ok(None));
}

/// `impl<T, U> Wanted<U> for T where for<F in fields(T)> F: Field<U>` is selectable
/// inside an owner with the same field bound. A missing bound, another container, or another
/// argument cannot justify the impl; the successful selection preserves the owner's parameters.
#[test]
fn owner_field_bounds_select_only_the_matching_impl() {
    use crate::fixture::Fx;
    use crate::infer::InferCtx;
    use crate::world::{PClause, Reads};

    let mut fx = Fx::new();
    let t = fx.param(0);
    let u = fx.param(1);
    let v = fx.param(2);
    let unit = fx.unit();
    let field = fx.tr("Field", 2, std::vec![], &[]);
    let wanted = fx.tr("Wanted", 2, std::vec![], &[]);
    let bound = PClause::ForFields { container: t, trait_: field, args: fx.list(&[u]) };
    let other_container = PClause::ForFields { container: v, trait_: field, args: fx.list(&[u]) };
    let other_arg = PClause::ForFields { container: t, trait_: field, args: fx.list(&[v]) };
    let imp = fx.imp("field-bounded impl", 2, t, Some((wanted, &[u])), &[], std::vec![bound]);
    let matching = fx.func("matching", 3, &[], unit, std::vec![bound]);
    let absent = fx.func("absent", 3, &[], unit, std::vec![]);
    let wrong_container = fx.func("wrong_container", 3, &[], unit, std::vec![other_container]);
    let wrong_arg = fx.func("wrong_arg", 3, &[], unit, std::vec![other_arg]);
    fx.finish();

    for (owner, expected) in [(matching, Some(imp)), (absent, None), (wrong_container, None), (wrong_arg, None)] {
        let alloc = Alloc::global();
        let mut ground = PortableTys::new_in(alloc);
        let t = ground.intern(PNode::Param(0)).expect("owner T");
        let u = ground.intern(PNode::Param(1)).expect("owner U");
        let mut cx = InferCtx::post_analysis_in(alloc);
        let mut reads = Reads::new_in(&fx, alloc);
        let selected = crate::instance::select_owning_owner(
            &mut cx, &mut reads, owner, None, wanted, &[t, u], &mut ground,
        ).expect("the owner-bound selection is decided");
        assert_eq!(selected.map(|(chosen, _)| chosen), expected, "owner {owner:?}");
        if let Some((_, args)) = selected {
            assert_eq!(ground.view().list(args).expect("the selected arguments"), &[t, u]);
        }
    }
}
