use super::*;
use blaze_lir::ty::Form;

pub(super) fn bits(words: &[u32]) -> u128 {
    words.iter().rev().fold(0u128, |value, &word| (value << 32) | u128::from(word))
}

impl<'a> Lower<'a, '_> {
    pub(super) fn intrinsic(
        &mut self,
        i: usize,
        name: AString<'a>,
        ret: LlType<'_>,
        args: &[(LlType<'_>, AString<'a>)],
    ) -> Result<(), LowerError> {
        let mut decl = format_in!(self.alloc, "declare {ret} @{name}(");
        self.ir.push_str("  ");
        if ret.1 != Ty::NONE {
            format_into!(&mut self.ir, "%v{i} = ");
        }
        format_into!(&mut self.ir, "call {ret} @{name}(");
        for (k, (ty, value)) in args.iter().enumerate() {
            if k != 0 {
                self.ir.push_str(", ");
                decl.push_str(", ");
            }
            format_into!(&mut self.ir, "{ty} {value}");
            format_into!(&mut decl, "{ty}");
        }
        self.ir.push_str(")\n");
        decl.push(')');
        self.declaration(format_in!(self.alloc, "@{name}"), decl)
    }

    fn math(&mut self, i: usize, op: Op) -> Result<(), LowerError> {
        let view = self.view;
        let mut o = view.operands(i);
        let a = o.value();
        let ty = view.ty(a);
        let ret = Ty(view.tys[i]);
        if ty.form() != Form::Scalar {
            return Err(self.error("vector overloaded LLVM intrinsic"));
        }
        let name = match op {
            Op::Rotl => "fshl",
            Op::Rotr => "fshr",
            Op::Clz => "ctlz",
            Op::Ctz => "cttz",
            Op::Popcnt => "ctpop",
            Op::Bswap => "bswap",
            Op::Bitrev => "bitreverse",
            Op::UaddOverflow => "uadd.with.overflow",
            Op::SaddOverflow => "sadd.with.overflow",
            Op::UsubOverflow => "usub.with.overflow",
            Op::SsubOverflow => "ssub.with.overflow",
            Op::UmulOverflow => "umul.with.overflow",
            Op::SmulOverflow => "smul.with.overflow",
            Op::UaddSat => "uadd.sat",
            Op::UsubSat => "usub.sat",
            Op::SaddSat => "sadd.sat",
            Op::SsubSat => "ssub.sat",
            Op::FminNum => "minnum",
            Op::FmaxNum => "maxnum",
            Op::Fminimum => "minimum",
            Op::Fmaximum => "maximum",
            Op::Fcopysign => "copysign",
            Op::Ftrunc => "trunc",
            Op::Nearest => "roundeven",
            Op::Fround => "round",
            Op::Fmuladd => "fmuladd",
            Op::FptoUi => "fptoui.sat",
            Op::FptoSi => "fptosi.sat",
            _ => op.name(),
        };
        let name = if matches!(op, Op::FptoUi | Op::FptoSi) {
            format_in!(self.alloc, "llvm.{name}.{}.{}", ret.elem().name(), ty.elem().name())
        } else {
            format_in!(self.alloc, "llvm.{name}.{}", ty.elem().name())
        };
        let operand = |v: Value| (LlType(view, view.ty(v)), format_in!(self.alloc, "%v{}", v.0));
        let mut args = Vec::with_capacity_in(3, self.alloc);
        args.push(operand(a));
        match op {
            Op::Rotl | Op::Rotr => {
                args.push(operand(a));
                args.push(operand(o.value()));
            }
            Op::Clz | Op::Ctz => {
                args.push((LlType(view, Ty::I1), String::from_str_in("false", self.alloc)));
            }
            _ => {
                for _ in op.info().layout.fields().iter().skip(1) {
                    args.push(operand(o.value()));
                }
            }
        }
        self.intrinsic(i, name, LlType(view, ret), &args)
    }

    fn division(&mut self, i: usize, op: Op) -> Result<(), LowerError> {
        let view = self.view;
        let mut operands = view.operands(i);
        let (a, b) = (operands.value().0, operands.value().0);
        let ty = Ty(view.tys[i]);
        if ty.form() != Form::Scalar {
            return Err(self.error("vector division with trapping lanes"));
        }
        let width = ty.elem().bits(view.head.ptr_bits);
        let t = LlType(view, ty);
        self.declaration(
            String::from_str_in("@llvm.trap", self.alloc),
            String::from_str_in("declare void @llvm.trap()", self.alloc),
        )?;
        format_into!(
            &mut self.ir,
            "  %zero{i} = icmp eq {t} %v{b}, 0\n  br i1 %zero{i}, label %divzero{i}, label %nonzero{i}\ndivzero{i}:\n  call void @llvm.trap(), !blaze.trap !{{i32 65280}}\n  unreachable\nnonzero{i}:\n"
        );
        if op == Op::Sdiv {
            format_into!(
                &mut self.ir,
                "  %min{i} = icmp eq {t} %v{a}, {}\n  %neg{i} = icmp eq {t} %v{b}, -1\n  %over{i} = and i1 %min{i}, %neg{i}\n  br i1 %over{i}, label %divover{i}, label %divok{i}\ndivover{i}:\n  call void @llvm.trap(), !blaze.trap !{{i32 65281}}\n  unreachable\ndivok{i}:\n",
                1u128 << (width - 1)
            );
        }
        if op == Op::Srem {
            format_into!(
                &mut self.ir,
                "  %neg{i} = icmp eq {t} %v{b}, -1\n  %safe{i} = select i1 %neg{i}, {t} 0, {t} %v{a}\n  %v{i} = srem {t} %safe{i}, %v{b}\n"
            );
        } else {
            format_into!(&mut self.ir, "  %v{i} = {} {t} %v{a}, %v{b}\n", op.name());
        }
        Ok(())
    }

    pub(super) fn instruction(&mut self, i: usize) -> Result<(), LowerError> {
        let view = self.view;
        let op = view.op(i);
        let mut o = view.operands(i);
        let ty = Ty(view.tys[i]);
        let t = LlType(view, ty);
        let typed =
            |v: Value| fmt::from_fn(move |f| write!(f, "{} %v{}", LlType(view, view.ty(v)), v.0));
        let out = &mut self.ir;
        match op {
            Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide => {
                let width = ty.elem().bits(view.head.ptr_bits);
                let value = match op {
                    Op::Iconst if width > 32 => (o.imm() as i32 as i128) as u128,
                    Op::Iconst | Op::Fconst => u128::from(o.imm()),
                    _ => bits(o.constant()),
                } & (u128::MAX >> (128 - width));
                if ty.elem().is_float() {
                    format_into!(out, "  %v{i} = bitcast i{width} ");
                } else {
                    format_into!(out, "  %v{i} = add {t} 0, ");
                }
                let start = out.len();
                format_into!(out, "{value}");
                self.literals.push(Literal { ty, bits: value, operand: start..out.len() });
                if ty.elem().is_float() {
                    format_into!(out, " to {t}");
                }
            }
            Op::Null => format_into!(out, "  %v{i} = getelementptr i8, ptr null, i32 0"),
            Op::Uninit => format_into!(out, "  %v{i} = select i1 true, {t} undef, {t} undef"),
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem => format_into!(
                out,
                "  %v{i} = {} {t} %v{}, %v{}",
                op.name(),
                o.value().0,
                o.value().0
            ),
            Op::Fneg | Op::Freeze => {
                format_into!(out, "  %v{i} = {} {t} %v{}", op.name(), o.value().0)
            }
            Op::Neg | Op::Not if ty.form() == Form::Scalar => {
                let (opcode, constant) = if op == Op::Neg { ("sub", "0") } else { ("xor", "-1") };
                format_into!(out, "  %v{i} = {opcode} {t} {constant}, %v{}", o.value().0);
            }
            Op::Shl | Op::Lshr | Op::Ashr if ty.form() == Form::Scalar => {
                let (a, b) = (o.value(), o.value());
                format_into!(
                    out,
                    "  %mask{i} = and {t} %v{}, {}\n  %v{i} = {} {t} %v{}, %mask{i}",
                    b.0,
                    ty.elem().bits(view.head.ptr_bits) - 1,
                    op.name(),
                    a.0
                );
            }
            Op::Sdiv | Op::Udiv | Op::Srem | Op::Urem => return self.division(i, op),
            Op::Rotl
            | Op::Rotr
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Umin
            | Op::Smin
            | Op::Umax
            | Op::Smax
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::Fma
            | Op::Fmuladd
            | Op::FptoUi
            | Op::FptoSi => return self.math(i, op),
            Op::Icmp | Op::Fcmp => {
                let cond = if op == Op::Icmp { o.int_cond().name() } else { o.float_cond().name() };
                format_into!(
                    out,
                    "  %v{i} = {} {cond} {}, %v{}",
                    op.name(),
                    typed(o.value()),
                    o.value().0
                );
            }
            Op::Select => format_into!(
                out,
                "  %v{i} = select {}, {}, {}",
                typed(o.value()),
                typed(o.value()),
                typed(o.value())
            ),
            Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fpext
            | Op::Fptrunc
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast
            | Op::PtrAddr
            | Op::Expose
            | Op::FromExposed => {
                let name = match op {
                    Op::PtrAddr | Op::Expose => "ptrtoint",
                    Op::FromExposed => "inttoptr",
                    _ => op.name(),
                };
                format_into!(out, "  %v{i} = {name} {} to {t}", typed(o.value()));
            }
            Op::PtrAdd => format_into!(
                out,
                "  %v{i} = getelementptr i8, {}, {}",
                typed(o.value()),
                typed(o.value())
            ),
            Op::PtrWithAddr => {
                let (ptr, addr) = (o.value(), o.value());
                let w = view.head.ptr_bits;
                format_into!(
                    out,
                    "  %addr{i} = ptrtoint ptr %v{} to i{w}\n  %offset{i} = sub i{w} %v{}, %addr{i}\n  %v{i} = getelementptr i8, ptr %v{}, i{w} %offset{i}",
                    ptr.0,
                    addr.0,
                    ptr.0
                );
            }
            Op::Project => {
                format_into!(out, "  %v{i} = extractvalue {}, {}", typed(o.value()), o.imm())
            }
            Op::Extract => {
                format_into!(out, "  %v{i} = extractelement {}, i32 {}", typed(o.value()), o.imm())
            }
            Op::Insert => format_into!(
                out,
                "  %v{i} = insertelement {}, {}, i32 {}",
                typed(o.value()),
                typed(o.value()),
                o.imm()
            ),
            Op::Assume => return Ok(()),
            Op::Call
            | Op::CallIndirect
            | Op::Invoke
            | Op::InvokeIndirect
            | Op::TailCall
            | Op::TailCallIndirect => return self.call(i, op),
            Op::Jump
            | Op::Br
            | Op::Switch
            | Op::Return
            | Op::Trap
            | Op::Unreachable
            | Op::Resume => return self.control(i, op),
            Op::SlotAddr
            | Op::SlotLive
            | Op::SlotDead
            | Op::DataAddr
            | Op::FuncAddr
            | Op::Load
            | Op::Store
            | Op::Memcpy
            | Op::Memmove
            | Op::Memset
            | Op::Memcmp
            | Op::AtomicLoad
            | Op::AtomicStore
            | Op::AtomicRmw
            | Op::Cmpxchg
            | Op::Fence => return self.memory(i, op),
            _ => return Err(self.error(op.name())),
        }
        out.push('\n');
        Ok(())
    }
}
