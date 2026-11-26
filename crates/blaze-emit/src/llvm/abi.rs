use super::*;
use blaze_lir::{
    Scalar,
    func::{Conv, Ext, FnAttrs, Inline, ParamKind, ParamRec, Sig, SigRec},
    op::Values,
    ty::Form,
};
use core::fmt::Write;

#[derive(Clone, Copy)]
pub(super) struct LlType<'v>(pub View<'v>, pub Ty);

impl fmt::Display for LlType<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(view, ty) = *self;
        if ty.form() == Form::Compound {
            f.write_str("{ ")?;
            for (i, &member) in view.members(ty).iter().enumerate() {
                if i != 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", Self(view, Ty(member)))?;
            }
            return f.write_str(" }");
        }
        let vector = ty.form() != Form::Scalar;
        if vector {
            f.write_str(if ty.form() == Form::Scalable { "<vscale x " } else { "<" })?;
            write!(f, "{} x ", ty.lanes())?;
        }
        f.write_str(match ty.elem() {
            Scalar::None => "void",
            Scalar::F16 => "half",
            Scalar::F32 => "float",
            Scalar::F64 => "double",
            Scalar::F128 => "fp128",
            s => s.name(),
        })?;
        if vector {
            f.write_char('>')?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct Signature<'v> {
    view: View<'v>,
    row: Sig<'v>,
    conv: &'static str,
}

fn extension(param: &ParamRec) -> &'static str {
    match Ext::from_byte(param.ext) {
        Some(Ext::Zext) => "zeroext",
        Some(Ext::Sext) => "signext",
        _ => "",
    }
}

pub(super) enum Args<'v> {
    Definition,
    Declaration,
    Call { values: Values<'v>, instruction: usize },
}

pub(super) enum AbiError {
    CallingConvention,
    PointeeLayout,
    ReturnAttributes,
}

impl AbiError {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::CallingConvention => "LLVM cannot express the LIR calling convention",
            Self::PointeeLayout => "an indirect ABI parameter has no pointee layout",
            Self::ReturnAttributes => {
                "unsupported attributes on a multi-register or indirect return"
            }
        }
    }
}

impl<'v> Signature<'v> {
    /// LLVM's ordinary musttail convention requires congruent prototypes and ABI
    /// attributes. LIR only requires the tail callee's result types to match.
    pub(super) fn tail_compatible(self, caller: Self) -> bool {
        let fields = |p: &ParamRec| {
            let indirect = self
                .view
                .facts(p)
                .filter(|_| ParamKind::from_byte(p.kind) != Some(ParamKind::Value))
                .map(|f| (f.size, f.align));
            (p.ty, p.kind, p.ext, indirect)
        };
        self.conv == caller.conv
            && self.row.bits == caller.row.bits
            && self.row.params.iter().map(fields).eq(caller.row.params.iter().map(fields))
            && self.row.rets.iter().map(fields).eq(caller.row.rets.iter().map(fields))
    }

    fn caller_copy(&self, p: &ParamRec) -> bool {
        // The current native LIR builder classifies AAPCS64 C aggregates over
        // 16 bytes as the address of a private caller copy (blaze-build/abi.rs).
        // LLVM's byval on arm64 instead passes the bytes on the argument stack.
        matches!(Conv::from_byte(self.row.conv), Some(Conv::C | Conv::Aapcs))
            && ParamKind::from_byte(p.kind) == Some(ParamKind::Byval)
    }

    fn copies(self) -> impl Iterator<Item = (usize, u32, u64)> + 'v {
        self.row.params.iter().enumerate().filter_map(move |(k, p)| {
            if !self.caller_copy(p) {
                return None;
            }
            let facts = self.view.facts(p)?;
            Some((k, facts.size, 1u64 << facts.align))
        })
    }

    pub(super) fn new(view: View<'v>, id: SigId) -> Result<Self, AbiError> {
        let row = view.sig(id);
        let conv = match Conv::from_byte(row.conv) {
            Some(Conv::C | Conv::Wasm | Conv::Aapcs) => "",
            Some(Conv::Win64) => "win64cc ",
            Some(Conv::SysV64) => "x86_64_sysvcc ",
            Some(Conv::PreserveMost) => "preserve_mostcc ",
            _ => return Err(AbiError::CallingConvention),
        };
        for param in row.params {
            if ParamKind::from_byte(param.kind) != Some(ParamKind::Value)
                && view.facts(param).is_none_or(|facts| facts.align > 32)
            {
                return Err(AbiError::PointeeLayout);
            }
        }
        if row.rets.iter().any(|p| ParamKind::from_byte(p.kind) != Some(ParamKind::Value))
            || (row.rets.len() > 1
                && row.rets.iter().any(|p| Ext::from_byte(p.ext) != Some(Ext::None)))
        {
            return Err(AbiError::ReturnAttributes);
        }
        Ok(Self { view, row, conv })
    }

    pub(super) fn result(self) -> impl fmt::Display + 'v {
        core::fmt::from_fn(move |f| match self.row.rets {
            [] => f.write_str("void"),
            [one] => write!(f, "{}", LlType(self.view, Ty(one.ty))),
            many => {
                f.write_str("{ ")?;
                for (i, p) in many.iter().enumerate() {
                    if i != 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", LlType(self.view, Ty(p.ty)))?;
                }
                f.write_str(" }")
            }
        })
    }
    pub(super) fn prefix(self) -> impl fmt::Display + 'v {
        core::fmt::from_fn(move |f| {
            f.write_str(self.conv)?;
            if let [p] = self.row.rets
                && !extension(p).is_empty()
            {
                write!(f, "{} ", extension(p))?;
            }
            write!(f, "{}", self.result())
        })
    }
    pub(super) fn call_type(self, out: &mut AString<'_>) {
        if self.row.bits & SigRec::VARIADIC != 0 {
            out.push_str(" (");
            for p in self.row.params {
                format_into!(out, "{}, ", LlType(self.view, Ty(p.ty)));
            }
            out.push_str("...)");
        }
    }
    pub(super) fn parameters(self, out: &mut AString<'_>, args: Args<'_>) {
        out.push('(');
        for (i, p) in self.row.params.iter().enumerate() {
            if i != 0 {
                out.push_str(", ");
            }
            format_into!(out, "{}", LlType(self.view, Ty(p.ty)));
            if !extension(p).is_empty() {
                format_into!(out, " {}", extension(p));
            }
            if let Some(facts) = self.view.facts(p)
                && let Some(kind @ (ParamKind::Sret | ParamKind::Byval)) =
                    ParamKind::from_byte(p.kind)
            {
                if !self.caller_copy(p) {
                    format_into!(out, " {}([{} x i8])", kind.name(), facts.size);
                }
                format_into!(out, " align {}", 1u64 << facts.align);
            }
            match args {
                Args::Definition => format_into!(out, " %v{i}"),
                Args::Call { instruction, .. } if self.caller_copy(p) => {
                    format_into!(out, " %copy{instruction}_{i}")
                }
                Args::Call { values, .. } => format_into!(out, " %v{}", values.get(i).0),
                Args::Declaration => {}
            }
        }
        if let Args::Call { values, .. } = args {
            for (i, v) in values.iter().enumerate().skip(self.row.params.len()) {
                if i != 0 {
                    out.push_str(", ");
                }
                format_into!(out, "{} %v{}", LlType(self.view, self.view.ty(v)), v.0);
            }
        } else if self.row.bits & SigRec::VARIADIC != 0 {
            if !self.row.params.is_empty() {
                out.push_str(", ");
            }
            out.push_str("...");
        }
        out.push(')');
    }
}

impl Lower<'_, '_> {
    /// Reserve call-site copies in the entry block, including for calls inside loops.
    pub(super) fn copy_slots(&mut self) -> Result<(), LowerError> {
        let view = self.view;
        if !view.sig_params.iter().any(|p| ParamKind::from_byte(p.kind) == Some(ParamKind::Byval)) {
            return Ok(());
        }
        for i in 0..view.len() {
            let mut o = view.operands(i);
            match view.op(i) {
                Op::Call | Op::Invoke | Op::TailCall => {
                    o.reference();
                }
                Op::CallIndirect | Op::InvokeIndirect | Op::TailCallIndirect => {
                    o.value();
                }
                _ => continue,
            }
            let sig = Signature::new(view, o.sig()).map_err(|e| LowerError {
                symbol: self.symbol,
                instruction: Some(i),
                reason: e.reason(),
            })?;
            for (k, size, align) in sig.copies() {
                format_into!(
                    &mut self.ir,
                    "  %copy{i}_{k} = alloca [{size} x i8], align {align}\n"
                );
            }
        }
        Ok(())
    }

    pub(super) fn copy_arguments(
        &mut self,
        sig: Signature<'_>,
        values: Values<'_>,
        i: usize,
        tail: bool,
    ) -> Result<(), LowerError> {
        for (k, size, align) in sig.copies() {
            if tail {
                return Err(self.error("musttail with a caller-owned aggregate copy"));
            }
            let v = values.get(k);
            format_into!(
                &mut self.ir,
                "  call void @llvm.memcpy.p0.p0.i32(ptr align {align} %copy{i}_{k}, ptr %v{}, i32 {size}, i1 false)\n",
                v.0
            );
            self.declaration(
                AString::from_str_in("@llvm.memcpy.p0.p0.i32", self.alloc),
                AString::from_str_in(
                    "declare void @llvm.memcpy.p0.p0.i32(ptr, ptr, i32, i1)",
                    self.alloc,
                ),
            )?;
        }
        Ok(())
    }
}

pub(super) fn attributes(out: &mut AString<'_>, bits: u32) {
    for (flag, name) in [
        (FnAttrs::NORETURN, " noreturn"),
        (FnAttrs::NOUNWIND, " nounwind"),
        (FnAttrs::COLD, " cold"),
        (FnAttrs::OPT_SIZE, " optsize"),
    ] {
        if bits & flag != 0 {
            out.push_str(name);
        }
    }
    if bits & FnAttrs::OPT_NONE != 0 {
        out.push_str(" noinline optnone");
    } else {
        out.push_str(match FnAttrs(bits).inline() {
            Inline::Always => " alwaysinline",
            Inline::Never => " noinline",
            Inline::Hint => " inlinehint",
            Inline::None => "",
        });
    }
}
