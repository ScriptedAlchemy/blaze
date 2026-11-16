//! First MIR adapter for the reference graph. Its finite instance list and effect tags
//! are supplied by these tests; it does not discover instances or issue effect proofs.
//! Unsupported representations, dispatch and drop leave explicit, body-owned gaps.

use std::alloc::{Allocator, Global};

use base_db::Db;
use hir_def::FunctionId;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate, PointerCast,
    next_solver::{GenericArgs, ParamEnv, StoredGenericArgs},
};
use hir_ty_mir::mir::{
    CastKind, LocalId, MirBody, MirPhase, MirSpan, Operand, OperandKind, ProjectionElem, Rvalue,
    StatementKind, StoredPlace, TerminatorKind,
    dispatch::{self, Body},
    return_slot,
};

use super::{fixture::crate_named, fns::fn_named};
use crate::callable::{Atom, Cell, Function, Graph, Node, Op};

struct Source<'db> {
    function: FunctionId,
    args: StoredGenericArgs,
    mir: &'db MirBody<'db>,
    effects: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GapKind {
    InstanceIdentity,
    NonRuntime,
    Projection,
    Representation,
    Aggregate,
    Static,
    Assembly,
    Intrinsic,
    Drop,
    Assert,
    Coroutine,
    ControlFlow,
}

struct Gap {
    owner: Function,
    span: Option<MirSpan>,
    kind: GapKind,
}

struct Extracted<A: Allocator> {
    graph: Graph<A>,
    functions: Vec<Function, A>,
    gaps: Vec<Gap, A>,
}

fn extract<A: Allocator + Clone>(sources: &[Source<'_>], alloc: A) -> Extracted<A> {
    let mut graph = Graph::new(alloc.clone());
    let mut functions = Vec::with_capacity_in(sources.len(), alloc.clone());
    let mut gaps = Vec::new_in(alloc.clone());
    for source in sources {
        functions.push(graph.function(source.mir.param_locals.len(), source.effects));
    }
    for (at, source) in sources.iter().enumerate() {
        let owner = functions[at];
        let mut locals = Vec::with_capacity_in(source.mir.locals.len(), alloc.clone());
        for _ in source.mir.locals.iter() {
            locals.push(graph.cell());
        }
        let mut lower = Lower {
            graph: &mut graph,
            alloc: alloc.clone(),
            sources,
            functions: &functions,
            locals: &locals,
            gaps: &mut gaps,
            owner,
            span: None,
        };
        if source.mir.owner != InferBodyId::from(source.function)
            || sources.iter().enumerate().any(|(other, it)| {
                other != at && it.function == source.function && it.args == source.args
            })
        {
            lower.gap(GapKind::InstanceIdentity);
            continue;
        }
        if source.mir.phase != MirPhase::Runtime {
            lower.gap(GapKind::NonRuntime);
            continue;
        }
        if source.mir.coroutine.is_some() {
            lower.gap(GapKind::Coroutine);
            continue;
        }
        for (index, &local) in source.mir.param_locals.iter().enumerate() {
            let from = lower.graph.param(owner, index);
            lower.write(Location::Local(lower.local(local)), from);
        }
        // Every block participates, including cleanup and alternative return paths.
        for (_, block) in source.mir.basic_blocks.iter() {
            for statement in &block.statements {
                lower.span = Some(statement.span);
                match &statement.kind {
                    StatementKind::Assign(place, rvalue) => {
                        let value = lower.rvalue(rvalue);
                        let location = lower.location(place);
                        lower.write(location, value);
                    }
                    StatementKind::InlineAsm(_) => lower.gap(GapKind::Assembly),
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            match &block.terminator {
                Some(terminator) => {
                    lower.span = Some(terminator.span);
                    lower.terminator(&terminator.kind);
                }
                None => lower.gap(GapKind::ControlFlow),
            }
        }
    }
    Extracted { graph, functions, gaps }
}

#[derive(Clone, Copy)]
enum Location {
    Local(Cell),
    Through(Node),
}

struct Lower<'a, 'db, A: Allocator> {
    graph: &'a mut Graph<A>,
    alloc: A,
    sources: &'a [Source<'db>],
    functions: &'a [Function],
    locals: &'a [Cell],
    gaps: &'a mut Vec<Gap, A>,
    owner: Function,
    span: Option<MirSpan>,
}

impl<A: Allocator + Clone> Lower<'_, '_, A> {
    fn gap(&mut self, kind: GapKind) {
        self.gaps.push(Gap { owner: self.owner, span: self.span, kind });
        let node = self.opaque();
        self.graph.add(self.owner, Op::Export(node));
    }

    fn opaque(&mut self) -> Node {
        let node = self.graph.node();
        self.graph.add(self.owner, Op::Seed(node, Atom::Opaque));
        node
    }

    fn copy(&mut self, from: Node) -> Node {
        let into = self.graph.node();
        self.graph.add(self.owner, Op::Copy { from, into });
        into
    }

    fn local(&self, local: LocalId) -> Cell {
        self.locals[local.into_raw().into_u32() as usize]
    }

    fn location(&mut self, place: &StoredPlace) -> Location {
        let mut location = Location::Local(self.local(place.local));
        for projection in place.projection.as_slice() {
            match projection {
                ProjectionElem::Deref => location = Location::Through(self.read(location)),
                ProjectionElem::Field(_)
                | ProjectionElem::Index(_)
                | ProjectionElem::ConstantIndex { .. }
                | ProjectionElem::Subslice { .. }
                | ProjectionElem::Downcast(_)
                | ProjectionElem::ReflectedField(_) => {
                    self.gap(GapKind::Projection);
                    return Location::Through(self.opaque());
                }
            }
        }
        location
    }

    fn read(&mut self, location: Location) -> Node {
        match location {
            Location::Local(cell) => self.graph.storage(cell),
            Location::Through(pointer) => {
                let into = self.graph.node();
                self.graph.add(self.owner, Op::Load { pointer, into });
                into
            }
        }
    }

    fn write(&mut self, location: Location, value: Node) {
        match location {
            Location::Local(cell) => {
                self.graph
                    .add(self.owner, Op::Copy { from: value, into: self.graph.storage(cell) });
            }
            Location::Through(pointer) => {
                self.graph.add(self.owner, Op::Store { pointer, value });
            }
        }
    }

    fn operand(&mut self, operand: &Operand) -> Node {
        if let Some((function, args)) = operand.fn_item() {
            let args = args.store();
            let atom = self
                .sources
                .iter()
                .position(|it| it.function == function && it.args == args)
                .map_or(Atom::Opaque, |index| Atom::Function(self.functions[index]));
            let node = self.graph.node();
            self.graph.add(self.owner, Op::Seed(node, atom));
            return node;
        }
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                let location = self.location(place);
                self.read(location)
            }
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => self.opaque(),
            OperandKind::Static(_) => {
                self.gap(GapKind::Static);
                self.opaque()
            }
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) -> Node {
        match rvalue {
            Rvalue::Use(value) => self.operand(value),
            Rvalue::CopyForDeref(place) => {
                let location = self.location(place);
                self.read(location)
            }
            Rvalue::Ref(_, place) => match self.location(place) {
                Location::Local(cell) => {
                    let node = self.graph.node();
                    self.graph.add(self.owner, Op::Seed(node, Atom::Address(cell)));
                    node
                }
                Location::Through(pointer) => self.copy(pointer),
            },
            Rvalue::Cast(kind, value, _) => {
                let value = self.operand(value);
                match kind {
                    CastKind::PointerCoercion(
                        PointerCast::ReifyFnPointer
                        | PointerCast::UnsafeFnPointer
                        | PointerCast::MutToConstPointer,
                    )
                    | CastKind::PtrToPtr
                    | CastKind::FnPtrToPtr => self.copy(value),
                    CastKind::IntToInt
                    | CastKind::FloatToInt
                    | CastKind::FloatToFloat
                    | CastKind::IntToFloat => {
                        self.copy(value);
                        self.opaque()
                    }
                    CastKind::PointerExposeAddress
                    | CastKind::PointerFromExposedAddress
                    | CastKind::Transmute
                    | CastKind::DynStar
                    | CastKind::PointerCoercion(
                        PointerCast::ClosureFnPointer(_)
                        | PointerCast::ArrayToPointer
                        | PointerCast::Unsize,
                    ) => {
                        self.gap(GapKind::Representation);
                        self.opaque()
                    }
                }
            }
            Rvalue::UnaryOp(_, value) => {
                let value = self.operand(value);
                self.copy(value);
                self.opaque()
            }
            Rvalue::BinaryOp(_, left, right) | Rvalue::CheckedBinaryOp(_, left, right) => {
                let left = self.operand(left);
                let right = self.operand(right);
                self.copy(left);
                self.copy(right);
                self.opaque()
            }
            Rvalue::Discriminant(place) => {
                let location = self.location(place);
                let value = self.read(location);
                self.copy(value);
                self.opaque()
            }
            Rvalue::Len(_) => self.opaque(),
            Rvalue::Aggregate(_, operands) if operands.is_empty() => self.opaque(),
            Rvalue::Repeat(_, _) | Rvalue::Aggregate(_, _) => {
                self.gap(GapKind::Aggregate);
                self.opaque()
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn call(&mut self, func: &Operand, args: &[Operand], into: Node) {
        let target = self.operand(func);
        let mut actuals = Vec::with_capacity_in(args.len(), self.alloc.clone());
        for arg in args {
            actuals.push(self.operand(arg));
        }
        self.graph.invoke(self.owner, target, &actuals, into);
    }

    fn terminator(&mut self, terminator: &TerminatorKind) {
        match terminator {
            TerminatorKind::Call { func, args, destination, intrinsic, .. } => {
                if intrinsic.is_some() {
                    self.gap(GapKind::Intrinsic);
                }
                let into = self.graph.node();
                self.call(func, args, into);
                let destination = self.location(destination);
                self.write(destination, into);
            }
            TerminatorKind::TailCall { func, args, .. } => {
                self.call(func, args, self.graph.returned(self.owner));
            }
            TerminatorKind::Return => {
                let value = self.graph.storage(self.local(return_slot()));
                self.graph.add(
                    self.owner,
                    Op::Copy { from: value, into: self.graph.returned(self.owner) },
                );
            }
            TerminatorKind::SwitchInt { discr, .. } => {
                let value = self.operand(discr);
                self.copy(value);
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::Unreachable => {}
            TerminatorKind::Drop { .. } | TerminatorKind::DropAndReplace { .. } => {
                self.gap(GapKind::Drop)
            }
            TerminatorKind::Assert { .. } => self.gap(GapKind::Assert),
            TerminatorKind::Yield { .. } | TerminatorKind::CoroutineDrop => {
                self.gap(GapKind::Coroutine)
            }
            TerminatorKind::Abort
            | TerminatorKind::UnwindResume
            | TerminatorKind::ComptimeFor { .. }
            | TerminatorKind::ComptimeNext { .. } => self.gap(GapKind::ControlFlow),
        }
    }
}

fn sources<'db>(db: &'db Db, named: &[(&str, u32)]) -> Vec<Source<'db>> {
    let krate = crate_named(db, "kernel");
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    let args = GenericArgs::new_from_slice(&[]).store();
    named
        .iter()
        .map(|&(name, effects)| {
            let function = fn_named(db, krate, name);
            let mir = dispatch::body_of(db, env.clone(), Body::Fn(function), args.clone()).unwrap();
            Source { function, args: args.clone(), mir, effects }
        })
        .collect()
}

#[test]
fn actual_mir_binds_factories_and_aliases_without_activating_a_dormant_gap() {
    let db = hir_expand::fixture::db(
        r#"
//- minicore: sized, copy, fn
//- /lib.rs crate:kernel
fn known() {}
fn factory() -> fn() { known }
fn put(slot: &mut fn(), value: fn()) { *slot = value; }
fn get(slot: &fn()) -> fn() { *slot }
fn dormant(bits: usize) -> *const fn() { bits as *const fn() }
pub fn run() {
    let _unused: fn(usize) -> *const fn() = dormant;
    let factory: fn() -> fn() = factory;
    let mut callback = factory();
    put(&mut callback, known);
    let callback = get(&callback);
    callback();
}
"#,
    )
    .into_db();
    blaze_query::db::attach(&db, || {
        let sources = sources(
            &db,
            &[("run", 0), ("factory", 0), ("known", 1), ("put", 0), ("get", 0), ("dormant", 0)],
        );
        let extracted = extract(&sources, Global);
        assert!(extracted.gaps.iter().any(|gap| gap.owner == extracted.functions[5]
            && gap.kind == GapKind::Representation
            && gap.span.is_some()));
        let solution = extracted.graph.solve(extracted.functions[0]).unwrap();
        assert!(solution.closed());
        assert_eq!(solution.effects(), 1);
        assert!(!extracted.graph.solve(extracted.functions[5]).unwrap().closed());
        assert!(extracted.graph.solve(extracted.functions[0]).unwrap().closed());
    });
}

#[test]
fn actual_mir_keeps_exposed_addresses_and_missing_callees_unproved() {
    let db = hir_expand::fixture::db(
        r#"
//- minicore: sized, copy, fn
//- /lib.rs crate:kernel
fn known() {}
pub fn missing() { known(); }
pub unsafe fn exposed(bits: usize) {
    let slot = bits as *const fn();
    unsafe { (*slot)(); }
}
"#,
    )
    .into_db();
    blaze_query::db::attach(&db, || {
        let sources = sources(&db, &[("missing", 0), ("exposed", 0)]);
        let extracted = extract(&sources, Global);
        assert!(!extracted.graph.solve(extracted.functions[0]).unwrap().closed());
        assert!(!extracted.graph.solve(extracted.functions[1]).unwrap().closed());
        assert!(
            extracted
                .gaps
                .iter()
                .any(|gap| gap.owner == extracted.functions[1]
                    && gap.kind == GapKind::Representation)
        );
    });
}
