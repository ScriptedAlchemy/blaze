//! `let` with a `ref` binding over a place: the initializer is built as a PLACE and the pattern
//! is bound where that place is, by the irrefutable binder's own projection
//! (matches/irrefutable.rs `bind_pattern_at`, the one home of it), never a copy.
//!
//! The rules, from the Reference:
//! - "Place Expressions and Value Expressions": "The following contexts are place expression
//!   contexts: ... The initializer of a `let` statement. The scrutinee of an `if let`, `match`,
//!   or `while let` expression ... The operand of a borrow ... The left operand of an
//!   assignment ... The operand of a field expression"; "When a place expression is evaluated
//!   in a value expression context, or is bound by value in a pattern, it denotes the value
//!   held in that memory location".
//! - "Identifier patterns": "`ref` ... binds to a reference to the matched place".
//! - [expr.move.movable-place]: "Only the following place expressions may be moved out of:
//!   Variables ... Temporary values. Fields of a place expression which can be moved out of and
//!   don't implement `Drop`. The result of dereferencing an expression with type `Box<T>`"; a
//!   value context reading a non-`Copy` place behind `&` is E0507 and never reaches the builder.
//!
//! So `let ref s = *r;` and `let (ref a, k) = *p;` (`p: &(G, i32)`) read nothing but `k`: `s`
//! and `a` hold the addresses of the places, and nothing moves. Before this, `*r` was evaluated
//! (`Task::Deref`, a load: refused for a type that needs drop) and walk.rs `plain_binding`
//! refused the `ref`. The match side (`match *self { V(ref x) => .. }`) is matches.rs
//! `bind_with` on the place walk.rs `Expr::Match` pushes (walk/moves.rs `scrutinee_in_place`).
//!
//! Taken here: a pattern with an explicit `ref`/`ref mut` binding, over an initializer that is
//! a place expression with no adjustment of its own. Every other by-value binding in it reads a
//! `Copy` part; a by-value binding of a part that needs drop would move out of the place (a
//! partial move of a local, or E0507 behind a reference) and is refused by name. A `ref` over a
//! VALUE initializer (`let ref x = f();`, whose temporary the `let` extends) stays the old
//! route's, refused there by name.

use core::alloc::Allocator;

use blaze_own::Vec;
use blaze_typeck::{ExprId, Pat, PatId};

use super::super::{Repr, V, Walk, malformed};
use super::At;
use crate::BuildError;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether `pat` contains an explicit `ref` or `ref mut` binding anywhere inside it.
    pub(in crate::walk) fn pat_has_ref(&self, pat: PatId) -> Result<bool, BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(pat);
        while let Some(p) = todo.pop() {
            match self.pat(p)? {
                Pat::Bind { ann, sub, .. } => {
                    if ann.by_ref.is_some() {
                        return Ok(true);
                    }
                    todo.extend(sub);
                }
                Pat::Ref { inner, .. } => todo.push(inner),
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|fp| fp.pat));
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    todo.extend(subs.iter().copied());
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Whether `let pat = init;` binds at `init`'s place: `pat` holds an explicit `ref` or
    /// `ref mut` binding and `init` is a place expression with no adjustment. A pure fact of the
    /// body, asked twice (walk.rs `stmt`, which then builds `init` by `Task::Place`, and
    /// `Task::Let`, which then binds here): both answers are the same.
    pub(in crate::walk) fn let_borrows_place(&self, pat: PatId, init: ExprId) -> Result<bool, BuildError> {
        if !self.adjust_rows(init).is_empty() || !self.is_place_expr(init)? {
            return Ok(false);
        }
        self.pat_has_ref(pat)
    }

    /// Refuses a by-value binding of a part that needs drop inside a `let` whose pattern borrows
    /// its initializer by `ref` (would require partial-move tracking out of the borrowed place).
    fn refuse_drop_by_value_in_ref_let(&mut self, pat: PatId) -> Result<(), BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(pat);
        while let Some(p) = todo.pop() {
            match self.pat(p)? {
                Pat::Bind { sub, .. } => {
                    let t = *self.checked.local_ty.get(p.ix()).ok_or(malformed("a binding without a type in the record"))?;
                    let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
                    if self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Err(BuildError::UnsupportedPat {
                            pat: p,
                            what: "a by-value binding of a part that needs drop in a `let` that borrows its initializer's place by `ref` (a move out of the place)",
                        });
                    }
                    todo.extend(sub);
                }
                Pat::Ref { inner, .. } => todo.push(inner),
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|fp| fp.pat));
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    todo.extend(subs.iter().copied());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Binds `pat` at `v`, the place of its `let`'s initializer (built by `Task::Place`): each
    /// `ref` binding the address of its part, each other binding a copy of a `Copy` part; the
    /// place is not the binder's own, so nothing in it moves or is dropped here.
    pub(in crate::walk) fn bind_let_place(&mut self, pat: PatId, v: V) -> Result<(), BuildError> {
        self.refuse_drop_by_value_in_ref_let(pat)?;
        let t = *self.checked.pat_ty.get(pat.ix()).ok_or(malformed("a pattern without a type in the record"))?;
        let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(pat, error))?;
        let at = match v {
            // An unsized place (`*s`, `s: &str`) is the wide pointer's memory, as matches.rs
            // `tested_place` takes it.
            V::Place(p, off) | V::Unsized(p, off) => (p, off),
            _ => return Err(malformed("a `let` initializer built as a place left a value")),
        };
        self.bind_pattern_at(pat, At { at, g, by_ref: false, copy: false }, false)
    }

    /// Binds `pat` (which contains an explicit `ref`/`ref mut` binding) over a VALUE initializer
    /// `init` whose evaluated value is `v`: materializes `v` in a dedicated stack slot, extends
    /// its temporary lifetime to the enclosing block if it needs drop (the Reference, "Temporary
    /// lifetime extension"), and binds `pat` at that place.
    pub(in crate::walk) fn bind_let_value_ref(&mut self, pat: PatId, init: ExprId, v: V) -> Result<(), BuildError> {
        self.refuse_drop_by_value_in_ref_let(pat)?;
        let t = self.adjusted_ty(init)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(init, error))?;
        let repr = self.repr(g).map_err(|error| Self::pat_ty_err(pat, error))?;
        let at = match v {
            V::Place(p, off) if self.live && self.is_fresh(init)? => (p, off),
            V::Unsized(..) => {
                return Err(BuildError::UnsupportedPat { pat, what: "a `ref` binding over an unsized temporary value" });
            }
            other => {
                let at = self.temp(repr);
                self.store(other, repr, at)?;
                at
            }
        };
        if self.needs_drop(g).map_err(|error| Self::pat_ty_err(pat, error))? {
            let Repr::Agg(l) = repr else { return Err(malformed("a value that needs drop not held as an aggregate")) };
            self.hold_extended_let_temp(init, at, l)?;
        }
        self.bind_pattern_at(pat, At { at, g, by_ref: false, copy: false }, false)
    }
}
