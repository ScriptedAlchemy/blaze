//! A boxed closure's object lifetime constrains its captures' outlives components, not the
//! bound predicate slots of a captured callable object. This is the `blaze_sim::replace_hook`
//! shape: keep a previous `Box<dyn for<'a, 'b> Fn(&'a Info<'b>) + 'static>` inside a new
//! boxed hook. Both bodies go through production lowering: the closure's actual findings
//! become requirements of its parent, which performs `Box<closure> -> Box<dyn Fn + 'static>`.
//!
//! The old flat capture loop is planted independently. It requires the captured hook's two
//! canonical binder placeholders to outlive `'static`. A short object lifetime and a real
//! borrowed capture after those predicate slots remain errors with the structural walk.

use blaze_flow::{BorrowFinding, Record};
use blaze_own::{Alloc, Vec};
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::regions::Region;
use blaze_typeck::{CaptureKind, ExprId, ItemKey, LangItem, Mutbl, Name, PTy, PNode};

use crate::callee_value_tests::{Lane, no_clauses};
use crate::region_fixture::{FnHead, P0, P1, P2, WithRegions, alloc, checked};
use crate::tests::Declared;
use crate::{AdtFacts, CalleeFacts, Facts, Need, Plant, Requirement, Variance};

const BOUND_A: Region = Region::Late { binder: 1, var: 0 };
const BOUND_B: Region = Region::Late { binder: 1, var: 1 };

#[derive(Clone, Copy)]
enum HookLifetime {
    Static,
    Short,
}

#[derive(Clone, Copy)]
enum ExtraCapture {
    None,
    BorrowedLocal,
}

#[derive(Clone, Copy)]
enum CaptureMode {
    Move,
    Borrow,
}

struct Case {
    hook: HookLifetime,
    extra: ExtraCapture,
    mode: CaptureMode,
    plant: Plant,
}

struct World {
    fx: Fx,
    lane: Lane,
    boxed: ItemKey,
    info: ItemKey,
    owner: ItemKey,
}

fn world(hook: HookLifetime) -> World {
    let mut fx = Fx::new();
    let mut lane = Lane::new();
    let sized = fx.tr("Sized", 1, no_clauses(), &[]);
    let unsize = fx.tr("Unsize", 2, no_clauses(), &[]);
    let coerce = fx.tr("CoerceUnsized", 2, no_clauses(), &[]);
    let fn_ = fx.tr("Fn", 2, no_clauses(), &[]);
    let fn_mut = fx.tr("FnMut", 2, no_clauses(), &[]);
    let fn_once = fx.tr("FnOnce", 2, no_clauses(), &["Output"]);
    for (lang, item) in [
        (LangItem::Sized, sized), (LangItem::Unsize, unsize),
        (LangItem::CoerceUnsized, coerce), (LangItem::Fn, fn_),
        (LangItem::FnMut, fn_mut), (LangItem::FnOnce, fn_once),
    ] {
        fx.lang(lang, item);
        lane.wide.sigs.decls.trait_(item, 0);
    }
    lane.fn_traits = [Some(fn_), Some(fn_mut), Some(fn_once)];
    lane.wide.sigs.decls.alias(fn_once, "Output", 0);

    let (p0, p1, unit) = (fx.param(0), fx.param(1), fx.unit());
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let shared = fx.r(Mutbl::Not, p0);
    let shared_copy = fx.imp("shared references are Copy", 1, shared, Some((copy, &[])), &[], no_clauses());
    fx.unsized_param(shared_copy, 0);
    let boxed = fx.strukt("Box", 1, &[]);
    let (box_t, box_u) = (fx.adt(boxed, &[p0]), fx.adt(boxed, &[p1]));
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let mut bounds = no_clauses();
    bounds.push(t_unsize_u);
    let coerce_impl = fx.imp("box unsizing", 2, box_t, Some((coerce, &[box_u])), &[], bounds);
    fx.unsized_param(coerce_impl, 0);
    fx.unsized_param(coerce_impl, 1);
    let box_new = fx.func("box_new", 1, &[p0], box_t, no_clauses());
    lane.wide.add(box_new, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[p0], output: box_t, slots: &[] });

    let info = fx.strukt("Info", 0, &[]);
    lane.wide.life = Some(info);
    let info_ty = fx.adt(info, &[]);
    let r_info = fx.r(Mutbl::Not, info_ty);
    let args = fx.tuple(&[r_info]);
    let args = fx.list(&[args]);
    let principal = fx.ty(PNode::ExTrait(fn_, args));
    let output = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args, term: unit });
    let predicates = fx.list(&[principal, output]);
    let hook_ty = fx.ty(PNode::Dynamic(predicates));
    let boxed_hook = fx.adt(boxed, &[hook_ty]);
    let r_hook = fx.r(Mutbl::Not, boxed_hook);

    let set_hook = fx.func("set_hook", 0, &[boxed_hook], unit, no_clauses());
    lane.wide.add(set_hook, FnHead {
        lifetimes: 0, late: &[], outlives: &[], inputs: &[boxed_hook], output: unit,
        slots: &[Region::Static, BOUND_A, BOUND_B, BOUND_A, BOUND_B],
    });
    let forward = fx.func("forward", 0, &[r_hook, r_info], unit, no_clauses());
    lane.wide.add(forward, FnHead {
        lifetimes: 4, late: &[0, 1, 2, 3], outlives: &[], inputs: &[r_hook, r_info], output: unit,
        slots: &[P0, P1, BOUND_A, BOUND_B, BOUND_A, BOUND_B, P2, Region::Param(3)],
    });
    let value = fx.strukt("Value", 0, &[]);
    let value_ty = fx.adt(value, &[]);
    let r_value = fx.r(Mutbl::Not, value_ty);
    let observe = fx.func("observe", 0, &[r_value], unit, no_clauses());
    lane.wide.add(observe, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[r_value], output: unit, slots: &[P0] });
    let owner = fx.func("install", 0, &[boxed_hook, value_ty], unit, no_clauses());
    let (lifetimes, late, object): (u32, &[u32], Region) = match hook {
        HookLifetime::Static => (0, &[], Region::Static),
        HookLifetime::Short => (1, &[0], P0),
    };
    lane.wide.add(owner, FnHead {
        lifetimes, late, outlives: &[], inputs: &[boxed_hook, value_ty], output: unit,
        slots: &[object, BOUND_A, BOUND_B, BOUND_A, BOUND_B],
    });
    fx.finish();
    World { fx, lane, boxed, info, owner }
}

/// The existing closure fixture facts, with the box marker and this closure's derived
/// requirements. Region columns are supplied by `WithRegions` after the real type check.
struct HookFacts<'a> {
    world: &'a World,
    closure: ExprId,
    requirements: &'a [Requirement],
}

impl Facts for HookFacts<'_> {
    fn regions(&self) -> &dyn crate::RegionFacts { self.world.lane.regions() }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> { self.world.lane.region_tys() }
    fn adt_variances(&self, k: ItemKey) -> Option<&[Variance]> {
        if k == self.world.boxed { Some(&[Variance::Co]) } else { self.world.lane.wide.adt_variances(k) }
    }
    fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> { self.world.lane.implied_bounds() }
    fn constant_initializer(&self) -> bool { false }
    fn fn_binders(&self) -> bool { true }
    fn async_bodies_checked(&self) -> bool { false }
    fn is_future(&self, _: PTy) -> Option<bool> { None }
    fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> { None }
    fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> { None }
    fn call_bounds(&self, _: ItemKey) -> Option<crate::CallBounds<'_>> { None }
    fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> { None }
    fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> { None }
    fn is_fn_trait(&self, k: ItemKey) -> bool { self.world.lane.is_fn_trait(k) }
    fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> { None }
    fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> { None }
    fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> { None }
    fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> { None }
    fn copy_args(&self, _: ItemKey) -> Option<&[bool]> { None }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        self.world.lane.adt(k).map(|facts| AdtFacts { is_box: k == self.world.boxed, ..facts })
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> { self.world.lane.callee(k) }
    fn closure_bodies_checked(&self) -> bool { true }
    fn closure_requirements(&self, closure: ExprId) -> &[Requirement] {
        if closure == self.closure { self.requirements } else { &[] }
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> { None }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> { None }
    fn param_copy(&self, _: u32) -> bool { false }
}

fn check(case: Case) -> Record<Alloc<'static>> {
    let w = world(case.hook);
    let mut b = Bb::new();
    let previous = b.param("previous");
    let local = b.param("local");
    let held = b.bind("held");
    let local_value = b.local(local);
    let borrow = b.borrow(Mutbl::Not, local_value);
    let keep = b.let_(held, None, borrow);
    let info = b.bind("info");
    let previous_value = b.local(previous);
    let previous_ref = b.borrow(Mutbl::Not, previous_value);
    let info_value = b.local(info);
    let forward = b.path(&["forward"]);
    let forwarded = b.call(forward, &[previous_ref, info_value]);
    let forwarded = b.semi(forwarded);
    let inner = match case.extra {
        ExtraCapture::None => b.block(&[forwarded], None),
        ExtraCapture::BorrowedLocal => {
            let observe = b.path(&["observe"]);
            let held_value = b.local(held);
            let observed = b.call(observe, &[held_value]);
            let observed = b.semi(observed);
            b.block(&[forwarded, observed], None)
        }
    };
    let closure = match case.mode {
        CaptureMode::Move => b.move_closure(&[info], inner),
        CaptureMode::Borrow => b.closure(&[info], inner),
    };
    // As in replace_hook, the closure is boxed into a local before set_hook supplies its
    // object type; its parameter is explicitly &Info<'_> rather than inferred from that call.
    let info_ty = b.ty(PNode::Adt(w.info, blaze_typeck::List::EMPTY));
    let r_info = b.ty(PNode::Ref(Mutbl::Not, info_ty));
    let make = b.path(&["box_new"]);
    let made = b.call(make, &[closure]);
    let hook = b.bind("hook");
    let made = b.let_(hook, None, made);
    let install = b.path(&["set_hook"]);
    let hook_value = b.local(hook);
    let installed = b.call(install, &[hook_value]);
    let installed = b.semi(installed);
    let root = b.block(&[keep, made, installed], None);
    let param_types = [Some(r_info)];
    let mut view = b.view(w.owner, root);
    assert_eq!(view.opt_types.len(), param_types.len(), "fixture has one annotated closure parameter");
    view.opt_types = &param_types;
    let mut ck = checked(&w.fx, &view);
    let mut fields = ck.env_fields_by::<blaze_typeck::Places>(closure);
    assert_eq!(fields.next().map(|f| f.base), Some(previous), "captured hook must precede the cursor control");
    if matches!(case.extra, ExtraCapture::BorrowedLocal) {
        assert_eq!(fields.next().map(|f| (f.base, f.how)), Some((held, CaptureKind::ByValue)));
    }
    assert!(fields.next().is_none(), "fixture has only the declared captures");

    let regions = w.lane.decls().lay(w.fx.t.view(), &mut ck.types);
    let initial = HookFacts { world: &w, closure, requirements: &[] };
    let initial = WithRegions { base: &initial, regions: &regions };
    let nested = match crate::lower_closure_external(&view, &ck, closure, &initial, alloc()) {
        Ok(nested) => nested,
        Err(error) => panic!("hook body must lower: {error:?}"),
    };
    let nested_record = match blaze_flow::check(&nested.lowered.flow, alloc()) {
        Ok(record) => record,
        Err(error) => panic!("hook body flow must check: {error:?}"),
    };
    assert!(nested_record.moves.is_empty(), "hook body move findings: {:?}", nested_record.moves);
    let mut requirements = Vec::new_in(alloc());
    for finding in &nested_record.borrows {
        let BorrowFinding::UnprovenOutlives { from, into, .. } = *finding else {
            panic!("hook body has an unrelated borrow error: {finding:?}");
        };
        let Some(requirement) = crate::requirement(&nested.externals, &nested.lowered.flow.known, from, into, alloc()) else {
            panic!("hook body requirement must map to its parent: {finding:?}; {:?}", nested.externals);
        };
        requirements.push(requirement);
    }
    for ordinal in [0, 1] {
        assert!(requirements.iter().any(|r| r.from == Need::DynPlaceholder(ordinal) || r.into == Need::DynPlaceholder(ordinal)),
            "the hook body's two quantified arguments must constrain its parent: {requirements:?}");
    }
    let facts = HookFacts { world: &w, closure, requirements: &requirements };
    let facts = WithRegions { base: &facts, regions: &regions };
    let parent = match crate::lower_planted(&view, &ck, &facts, case.plant, alloc()) {
        Ok(parent) => parent,
        Err(error) => panic!("hook owner must lower: {error:?}"),
    };
    assert!(parent.coarse.is_none(), "the owner must use exact region relations: {:?}", parent.coarse);
    match blaze_flow::check(&parent.flow, alloc()) {
        Ok(record) => record,
        Err(error) => panic!("hook owner flow must check: {error:?}"),
    }
}

#[test]
fn a_static_captured_hrtb_hook_unsizes_in_its_parent() {
    let record = check(Case { hook: HookLifetime::Static, extra: ExtraCapture::None, mode: CaptureMode::Move, plant: Plant::None });
    assert!(record.moves.is_empty() && record.borrows.is_empty(), "moves: {:?}; borrows: {:?}", record.moves, record.borrows);
    assert!(record.proof.is_some(), "the exact production parent flow must have a proof");
}

#[test]
fn flattened_capture_predicates_reject_the_static_hook() {
    let record = check(Case { hook: HookLifetime::Static, extra: ExtraCapture::None, mode: CaptureMode::Move, plant: Plant::CapturePredicatesOutlive });
    assert!(record.moves.is_empty(), "planted defect must be a region finding: {:?}", record.moves);
    assert!(record.borrows.iter().any(|f| matches!(f, BorrowFinding::UnprovenOutlives { .. })), "old flattening must reject bound predicate slots: {:?}", record.borrows);
    assert!(record.proof.is_none());
}

#[test]
fn a_short_captured_hook_still_cannot_become_static() {
    let record = check(Case { hook: HookLifetime::Short, extra: ExtraCapture::None, mode: CaptureMode::Move, plant: Plant::None });
    assert!(record.moves.is_empty(), "short object lifetime is a region finding: {:?}", record.moves);
    assert!(record.borrows.iter().any(|f| matches!(f, BorrowFinding::UnprovenOutlives { .. })), "the captured object's real lifetime must outlive the new object: {:?}", record.borrows);
    assert!(record.proof.is_none());
}

#[test]
fn a_borrow_after_captured_predicates_still_cannot_escape() {
    let record = check(Case { hook: HookLifetime::Static, extra: ExtraCapture::BorrowedLocal, mode: CaptureMode::Move, plant: Plant::None });
    assert!(record.moves.is_empty(), "the following capture must retain its borrow: {:?}", record.moves);
    assert!(record.borrows.iter().any(|f| matches!(f, BorrowFinding::Invalidated { .. })), "skipping a predicate subtree must retain the later local borrow: {:?}", record.borrows);
    assert!(record.proof.is_none());
}

#[test]
fn a_hook_captured_by_reference_still_cannot_escape() {
    let record = check(Case { hook: HookLifetime::Static, extra: ExtraCapture::None, mode: CaptureMode::Borrow, plant: Plant::None });
    assert!(record.moves.is_empty(), "the hook is borrowed: {:?}", record.moves);
    assert!(record.borrows.iter().any(|f| matches!(f, BorrowFinding::Invalidated { .. })), "the capture's own reference must satisfy the object lifetime: {:?}", record.borrows);
    assert!(record.proof.is_none());
}
