use super::*;
use blaze_lir::{
    BlockKind,
    func::SigRec,
    op::{Target as Branch, Values},
};

/// Give each argument-bearing edge its own predecessor. Two arms of the same
/// terminator can branch to the same block with different arguments.
pub(super) struct Edges<'a, 'v> {
    incoming: AVec<'a, AVec<'a, (usize, usize, Values<'v>)>>,
}

impl<'a, 'v> Edges<'a, 'v> {
    pub(super) fn new(view: View<'v>, alloc: Alloc<'a>) -> Self {
        let mut incoming = Vec::with_capacity_in(view.blocks(), alloc);
        for _ in 0..view.blocks() {
            incoming.push(Vec::new_in(alloc));
        }
        for b in 0..view.blocks() {
            let i = view.terminator(Block(b as u32));
            let mut o = view.operands(i);
            let mut edge = |k, t: Branch<'v>| {
                if !t.args.0.is_empty() {
                    incoming[t.block.index()].push((i, k, t.args));
                }
            };
            match view.op(i) {
                Op::Jump => edge(0, o.target()),
                Op::Br => {
                    o.value();
                    edge(0, o.target());
                    edge(1, o.target());
                }
                Op::Switch => {
                    let v = o.value();
                    edge(0, o.target());
                    for (k, (_, t)) in o.cases(v).enumerate() {
                        edge(k + 1, t);
                    }
                }
                _ => {}
            }
        }
        Self { incoming }
    }
    pub(super) fn write(&self, ir: &mut AString<'_>) {
        for (b, incoming) in self.incoming.iter().enumerate() {
            for &(i, k, _) in incoming.iter() {
                format_into!(ir, "e{i}_{k}:\n  br label %b{b}\n");
            }
        }
    }
}

fn label(out: &mut AString<'_>, i: usize, k: usize, target: Branch<'_>) {
    if target.args.0.is_empty() {
        format_into!(out, "label %b{}", target.block.0);
    } else {
        format_into!(out, "label %e{i}_{k}");
    }
}

impl Lower<'_, '_> {
    pub(super) fn block_params(&mut self, block: Block) -> Result<(), LowerError> {
        if block.0 == 0 {
            return Ok(());
        }
        let params = self.view.params(block);
        let kind = self.view.block_kind(block);
        if kind.is_landing() {
            let clause = if kind == BlockKind::Cleanup { "cleanup" } else { "catch ptr null" };
            format_into!(
                &mut self.ir,
                "  %pad{} = landingpad {{ ptr, i32 }} {clause}\n  %v{} = extractvalue {{ ptr, i32 }} %pad{}, 0\n",
                block.0,
                params.start,
                block.0
            );
            return Ok(());
        }
        for (k, i) in params.enumerate() {
            let ty = LlType(self.view, Ty(self.view.tys[i]));
            let incoming = &self.edges.incoming[block.index()];
            if incoming.is_empty() {
                format_into!(&mut self.ir, "  %v{i} = freeze {ty} poison\n");
                continue;
            }
            format_into!(&mut self.ir, "  %v{i} = phi {ty} ");
            for (n, &(from, arm, args)) in incoming.iter().enumerate() {
                if n != 0 {
                    self.ir.push_str(", ");
                }
                format_into!(&mut self.ir, "[ %v{}, %e{from}_{arm} ]", args.get(k).0);
            }
            self.ir.push('\n');
        }
        Ok(())
    }

    pub(super) fn control(&mut self, i: usize, op: Op) -> Result<(), LowerError> {
        let view = self.view;
        let mut o = view.operands(i);
        let ir = &mut self.ir;
        match op {
            Op::Jump => {
                ir.push_str("  br ");
                label(ir, i, 0, o.target());
            }
            Op::Br => {
                format_into!(ir, "  br i1 %v{}, ", o.value().0);
                label(ir, i, 0, o.target());
                ir.push_str(", ");
                label(ir, i, 1, o.target());
            }
            Op::Switch => {
                let v = o.value();
                let ty = LlType(view, view.ty(v));
                format_into!(ir, "  switch {ty} %v{}, ", v.0);
                label(ir, i, 0, o.target());
                ir.push_str(" [\n");
                for (k, (value, target)) in o.cases(v).enumerate() {
                    format_into!(ir, "    {ty} {}, ", instruction::bits(value));
                    label(ir, i, k + 1, target);
                    ir.push('\n');
                }
                ir.push(']');
            }
            Op::Return => {
                let values = o.values();
                match values.0 {
                    [] => ir.push_str("  ret void"),
                    [v] => format_into!(ir, "  ret {} %v{v}", LlType(view, view.ty(Value(*v)))),
                    _ => {
                        let sig = Signature::new(view, SigId(view.head.sig)).map_err(|reason| {
                            LowerError { symbol: self.symbol, instruction: Some(i), reason }
                        })?;
                        let ty = sig.result();
                        for (k, v) in values.iter().enumerate() {
                            format_into!(ir, "  %r{i}_{k} = insertvalue {ty} ");
                            if k == 0 {
                                ir.push_str("poison");
                            } else {
                                format_into!(ir, "%r{i}_{}", k - 1);
                            }
                            format_into!(ir, ", {} %v{}, {k}\n", LlType(view, view.ty(v)), v.0);
                        }
                        format_into!(ir, "  ret {ty} %r{i}_{}", values.0.len() - 1);
                    }
                }
            }
            Op::Unreachable | Op::Trap => {
                let code = if op == Op::Trap { o.imm() } else { 0xff02 };
                // The AArch64 emitter uses BRK's 16-bit immediate as the trap identity.
                // This module can target wasm too: llvm.trap is portable but cannot
                // carry a trap code. Keep the code in the intrinsic's metadata.
                format_into!(
                    ir,
                    "  call void @llvm.trap(), !blaze.trap !{{i32 {code}}}\n  unreachable"
                );
                self.declaration(
                    String::from_str_in("@llvm.trap", self.alloc),
                    String::from_str_in("declare void @llvm.trap()", self.alloc),
                )?;
            }
            Op::Resume => {
                format_into!(
                    ir,
                    "  call void @_Unwind_Resume(ptr %v{})\n  unreachable",
                    o.value().0
                );
                self.declaration(
                    String::from_str_in("@_Unwind_Resume", self.alloc),
                    String::from_str_in("declare void @_Unwind_Resume(ptr)", self.alloc),
                )?;
            }
            _ => return Err(self.error(op.name())),
        }
        self.ir.push('\n');
        Ok(())
    }

    pub(super) fn call(&mut self, i: usize, op: Op) -> Result<(), LowerError> {
        let view = self.view;
        let mut o = view.operands(i);
        let id = match op {
            Op::Call | Op::Invoke | Op::TailCall => Some(o.reference()),
            _ => None,
        };
        let callee = if let Some(id) = id {
            self.names[id.index()].clone()
        } else {
            format_in!(self.alloc, "%v{}", o.value().0)
        };
        let sig_id = o.sig();
        let signature = Signature::new(view, sig_id).map_err(|e| self.error(e))?;
        let unwind = matches!(op, Op::Invoke | Op::InvokeIndirect).then(|| (o.block(), o.block()));
        let args = o.values();
        let tail = matches!(op, Op::TailCall | Op::TailCallIndirect);
        if tail && view.sig(sig_id).bits & SigRec::VARIADIC != 0 {
            return Err(self.error("variadic musttail call"));
        }
        if let Some(id) = id {
            let target = self.references[id.index()]
                .ok_or_else(|| self.error("a direct call names local data"))?;
            if target.space != Space::Func || target.symbol != self.symbol {
                let mut decl = format_in!(self.alloc, "declare {} {callee}", signature.prefix());
                signature.parameters(&mut decl, abi::Args::Declaration);
                self.declaration(callee.clone(), decl)?;
                if target.space == Space::Func && !self.calls.contains(&target.symbol) {
                    self.calls.push(target.symbol);
                }
            }
        }
        self.ir.push_str("  ");
        if !view.sig(sig_id).rets.is_empty() {
            format_into!(&mut self.ir, "%v{i} = ");
        }
        let mnemonic = if unwind.is_some() {
            "invoke"
        } else if tail {
            "musttail call"
        } else {
            "call"
        };
        format_into!(&mut self.ir, "{mnemonic} {}", signature.prefix());
        signature.call_type(&mut self.ir);
        format_into!(&mut self.ir, " {callee}");
        signature.parameters(&mut self.ir, abi::Args::Call(args));
        if let Some((normal, landing)) = unwind {
            format_into!(&mut self.ir, " to label %b{} unwind label %b{}", normal.0, landing.0);
        }
        if tail {
            if view.sig(sig_id).rets.is_empty() {
                self.ir.push_str("\n  ret void");
            } else {
                format_into!(&mut self.ir, "\n  ret {} %v{i}", signature.result());
            }
        }
        self.ir.push('\n');
        Ok(())
    }
}
