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
    Call(Values<'v>),
}

impl<'v> Signature<'v> {
    pub(super) fn new(view: View<'v>, id: SigId) -> Result<Self, &'static str> {
        let row = view.sig(id);
        let conv = match Conv::from_byte(row.conv) {
            Some(Conv::C | Conv::Wasm | Conv::Aapcs) => "",
            Some(Conv::Win64) => "win64cc ",
            Some(Conv::SysV64) => "x86_64_sysvcc ",
            Some(Conv::PreserveMost) => "preserve_mostcc ",
            _ => return Err("LLVM cannot express the LIR calling convention"),
        };
        for param in row.params {
            if param.kind != ParamKind::Value as u8 && view.facts(param).is_none() {
                return Err("an indirect ABI parameter has no pointee layout");
            }
        }
        if row.rets.iter().any(|p| p.kind != ParamKind::Value as u8)
            || (row.rets.len() > 1 && row.rets.iter().any(|p| p.ext != Ext::None as u8))
        {
            return Err("unsupported attributes on a multi-register or indirect return");
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
                format_into!(
                    out,
                    " {}([{} x i8]) align {}",
                    kind.name(),
                    facts.size,
                    1u64 << facts.align
                );
            }
            match args {
                Args::Definition => format_into!(out, " %v{i}"),
                Args::Call(v) => format_into!(out, " %v{}", v.get(i).0),
                Args::Declaration => {}
            }
        }
        if let Args::Call(values) = args {
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
