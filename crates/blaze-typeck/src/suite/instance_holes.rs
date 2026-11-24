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
fn owner_field_bounds_select_only_the_matching_impl() -> Result<(), crate::world::SplitSelf> {
    use crate::ids::{LangItem, PathIdx, ScopeId};
    use crate::infer::InferCtx;
    use crate::portable::{PTy, PTys};
    use crate::world::{
        Adt, ArgsAfterSelf, AssocItem, AssocParent, ClosureFacts, ConstHolds, ConstParams, Def,
        Fetch, FnSig, Hidden, ImplHeader, ImplPolarity, Missing, PClause, Predicates, Reads, Seal,
        TraitDef, TraitDefaults, TraitFlags, TraitMembers, TypeAttrs, World,
    };

    const FIELD: ItemKey = ItemKey(1);
    const WANTED: ItemKey = ItemKey(2);
    const IMP: ItemKey = ItemKey(3);
    const MATCHING: ItemKey = ItemKey(4);
    const ABSENT: ItemKey = ItemKey(5);
    const WRONG_CONTAINER: ItemKey = ItemKey(6);
    const WRONG_ARG: ItemKey = ItemKey(7);

    // These facts borrow fixed tables: building the fixture itself needs no collections.
    // Any question outside this selection's facts is a hole, never an empty answer.
    struct FieldBoundsWorld<'a> {
        types: PTys<'a>,
        owners: [(ItemKey, &'a [PClause]); 4],
        impl_predicates: &'a [PClause],
        header: ImplHeader<'a>,
    }
    impl World for FieldBoundsWorld<'_> {
        fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
            let clauses = if item == IMP {
                self.impl_predicates
            } else {
                self.owners.iter().find(|(key, _)| *key == item).map(|(_, clauses)| *clauses).ok_or(Missing)?
            };
            Ok(Predicates { clauses, types: self.types })
        }
        fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
            if trait_ == WANTED && shape == Shape::Param { Ok(&[IMP]) } else { Err(Missing) }
        }
        fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
            if item == IMP { Ok(Some(self.header)) } else { Err(Missing) }
        }
        fn trait_def(&self, _: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
            if trait_ != FIELD && trait_ != WANTED {
                return Err(Missing);
            }
            Ok(Some(TraitDef {
                generics: 2,
                flags: TraitFlags(0),
                lang: None,
                supertraits: &[],
                assoc_types: &[],
                types: self.types,
                members: TraitMembers::Known(&[]),
                defaults: TraitDefaults::Known(&[None]),
            }))
        }
        fn stops_at_holes(&self) -> bool { true }
        fn fn_sig(&self, _: Seal, _: ItemKey) -> Fetch<Option<FnSig<'_>>> { Err(Missing) }
        fn adt(&self, _: Seal, _: ItemKey) -> Fetch<Option<Adt<'_>>> { Err(Missing) }
        fn inherent(&self, _: Seal, _: Shape, _: Name) -> Fetch<&[AssocItem]> { Err(Missing) }
        fn assoc_item(&self, _: Seal, _: ItemKey, _: Name) -> Fetch<Option<AssocItem>> { Err(Missing) }
        fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> { Err(Missing) }
        fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, _: &[Name]) -> Fetch<crate::path_miss::PathAnswer> {
            Err(Missing)
        }
        fn lang_item(&self, _: Seal, _: LangItem) -> Fetch<Option<ItemKey>> { Err(Missing) }
        fn lang_path(&self, _: Seal, _: LangItem) -> Fetch<Option<Def>> { Err(Missing) }
        fn const_value(&self, _: Seal, _: ItemKey) -> Fetch<Option<u64>> { Err(Missing) }
        fn opaque_hidden(&self, _: Seal, _: ItemKey) -> Fetch<Option<Hidden<'_>>> { Err(Missing) }
        fn closure(&self, _: Seal, _: ItemKey, _: u32) -> Fetch<Option<ClosureFacts<'_>>> { Err(Missing) }
        fn attrs(&self, _: Seal, _: ItemKey) -> Fetch<TypeAttrs> { Err(Missing) }
        fn assoc_parent(&self, _: Seal, _: ItemKey) -> Fetch<Option<AssocParent>> { Err(Missing) }
        fn const_holds(&self, _: Seal, _: ItemKey, _: &[PTy], _: PTys<'_>) -> Fetch<ConstHolds> { Err(Missing) }
        fn const_params(&self, _: Seal, _: ItemKey) -> Fetch<ConstParams<'_>> { Err(Missing) }
    }

    let nodes = [PNode::Param(0), PNode::Param(1), PNode::Param(2)];
    let elements = [PTy(0), PTy(1), PTy(2)];
    let types = PTys::flat(&nodes, &elements);
    let bound = [PClause::ForFields { container: PTy(0), trait_: FIELD, args: List { start: 1, len: 1 } }];
    let other_container = [PClause::ForFields { container: PTy(2), trait_: FIELD, args: List { start: 1, len: 1 } }];
    let other_arg = [PClause::ForFields { container: PTy(0), trait_: FIELD, args: List { start: 2, len: 1 } }];
    let world = FieldBoundsWorld {
        types,
        owners: [(MATCHING, &bound), (ABSENT, &[]), (WRONG_CONTAINER, &other_container), (WRONG_ARG, &other_arg)],
        impl_predicates: &bound,
        header: ImplHeader {
            generics: 2,
            self_ty: PTy(0),
            trait_: Some(WANTED),
            trait_args: ArgsAfterSelf::split(types, List { start: 0, len: 2 }, PTy(0))?,
            assoc_types: &[],
            types,
            polarity: ImplPolarity::Positive,
        },
    };

    let alloc = blaze_own::testing::heap().alloc();
    for (owner, expected) in [(MATCHING, Some(IMP)), (ABSENT, None), (WRONG_CONTAINER, None), (WRONG_ARG, None)] {
        let mut ground = PortableTys::new_in(alloc);
        let t = ground.intern(PNode::Param(0)).expect("owner T");
        let u = ground.intern(PNode::Param(1)).expect("owner U");
        let mut cx = InferCtx::post_analysis_in(alloc);
        let mut reads = Reads::new_in(&world, alloc);
        let selected = crate::instance::select_owning_owner(
            &mut cx, &mut reads, owner, None, WANTED, &[t, u], &mut ground,
        ).expect("the owner-bound selection is decided");
        assert!(!reads.holed(), "owner {owner:?} must be selected from complete facts");
        assert_eq!(selected.map(|(chosen, _)| chosen), expected, "owner {owner:?}");
        if let Some((_, args)) = selected {
            assert_eq!(ground.view().list(args).expect("the selected arguments"), &[t, u]);
        }
    }
    Ok(())
}
