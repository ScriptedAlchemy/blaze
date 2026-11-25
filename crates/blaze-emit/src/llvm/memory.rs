use super::*;
use blaze_lir::{Flags, Ordering, func::DataId};

fn order(o: Option<Ordering>) -> &'static str {
    match o {
        Some(Ordering::Relaxed) => "monotonic",
        Some(Ordering::Acquire) => "acquire",
        Some(Ordering::Release) => "release",
        Some(Ordering::AcqRel) => "acq_rel",
        Some(Ordering::SeqCst) => "seq_cst",
        None => unreachable!("verified atomic ordering"),
    }
}

fn bytes(out: &mut AString<'_>, data: &[u8]) {
    format_into!(out, "[{} x i8] c\"", data.len());
    for b in data {
        format_into!(out, "\\{b:02X}");
    }
    out.push('"');
}

impl Lower<'_, '_> {
    pub(super) fn globals(&mut self, own: &str) -> Result<(), LowerError> {
        let view = self.view;
        for (i, row) in view.refs.iter().enumerate() {
            let name = &self.names[i];
            if name == own || self.declarations.contains_key(name) {
                continue;
            }
            match RefKind::from_word(row.kind) {
                Some(RefKind::Static | RefKind::Vtable) => self.declaration(
                    name.clone(),
                    format_in!(self.alloc, "{name} = external global i8"),
                )?,
                Some(RefKind::Func) => self.declaration(
                    name.clone(),
                    format_in!(self.alloc, "declare void {name}(...)"),
                )?,
                _ => {}
            }
        }
        for (k, row) in view.datas.iter().enumerate() {
            let id = DataId(k as u32);
            let data = view.data_bytes_of(id);
            let relocs = view.data_relocs_of(id);
            let positions = view.data_positions_of(id);
            // Placement writes the line/column after optimization. LLVM may neither
            // fold those loads to zero nor discard the symbol the position rows name.
            let placement = !positions.is_empty();
            let name = if placement {
                format_in!(
                    self.alloc,
                    "@blaze_pos_{}_{k}",
                    LinkName(Target { space: Space::Func, symbol: self.symbol })
                )
            } else {
                format_in!(self.alloc, "@d{k}")
            };
            if placement {
                format_into!(&mut self.ir, "@d{k} = private alias i8, ptr {name}\n");
            }
            let storage =
                if placement { "hidden externally_initialized global" } else { "private constant" };
            format_into!(&mut self.ir, "{name} = {storage} ");
            if relocs.is_empty() {
                bytes(&mut self.ir, data);
            } else {
                let mut layout = String::from_str_in("<{ ", self.alloc);
                let mut value = String::from_str_in("<{ ", self.alloc);
                let mut at = 0;
                for rel in relocs {
                    let offset = rel.offset as usize;
                    let chunk = data
                        .get(at..offset)
                        .ok_or_else(|| self.error("overlapping data relocations"))?;
                    if at != 0 {
                        layout.push_str(", ");
                        value.push_str(", ");
                    }
                    if !chunk.is_empty() {
                        format_into!(&mut layout, "[{} x i8], ", chunk.len());
                        bytes(&mut value, chunk);
                        value.push_str(", ");
                    }
                    if view.reference(RefId(rel.target)).kind == RefKind::Tls as u32 {
                        return Err(self.error("a data relocation to a Blaze thread-local index"));
                    }
                    layout.push_str("ptr");
                    format_into!(
                        &mut value,
                        "ptr getelementptr (i8, ptr {}, i64 {})",
                        self.names[rel.target as usize],
                        rel.addend
                    );
                    at = offset + (view.head.ptr_bits / 8) as usize;
                }
                let tail =
                    data.get(at..).ok_or_else(|| self.error("out-of-bounds data relocation"))?;
                if !tail.is_empty() {
                    format_into!(&mut layout, ", [{} x i8]", tail.len());
                    value.push_str(", ");
                    bytes(&mut value, tail);
                }
                format_into!(&mut self.ir, "{layout} }}> {value} }}>");
            }
            let align = 1u64
                .checked_shl(row.align)
                .ok_or_else(|| self.error("data alignment exceeds LLVM limit"))?;
            format_into!(&mut self.ir, ", align {align}\n");
        }
        Ok(())
    }

    pub(super) fn memory(&mut self, i: usize, op: Op) -> Result<(), LowerError> {
        let view = self.view;
        let mut o = view.operands(i);
        let ty = Ty(view.tys[i]);
        let t = LlType(view, ty);
        let flags = view.flags(i);
        let volatile = if flags.contains(Flags::VOLATILE) { " volatile" } else { "" };
        let align = 1u64
            .checked_shl(view.mem_attrs(i).map_or(0, |a| u32::from(a.align)))
            .ok_or_else(|| self.error("memory alignment exceeds LLVM limit"))?;
        let ir = &mut self.ir;
        match op {
            Op::SlotLive | Op::SlotDead => return Ok(()),
            Op::SlotAddr => {
                format_into!(ir, "  %v{i} = getelementptr i8, ptr %s{}, i32 0", o.slot().0)
            }
            Op::DataAddr | Op::FuncAddr => format_into!(
                ir,
                "  %v{i} = getelementptr i8, ptr {}, i32 0",
                self.names[o.reference().index()]
            ),
            Op::Load | Op::Store => {
                let stored = (op == Op::Store).then(|| o.value());
                let base = o.value();
                format_into!(
                    ir,
                    "  %p{i} = getelementptr i8, ptr %v{}, i32 {}\n",
                    base.0,
                    o.offset()
                );
                if let Some(value) = stored {
                    format_into!(
                        ir,
                        "  store{volatile} {} %v{}, ptr %p{i}, align {align}",
                        LlType(view, view.ty(value)),
                        value.0
                    );
                } else {
                    format_into!(ir, "  %v{i} = load{volatile} {t}, ptr %p{i}, align {align}");
                }
            }
            Op::Memcpy | Op::Memmove | Op::Memset => {
                let (dst, src, len) = (o.value(), o.value(), o.value());
                let ptrs = if op == Op::Memset { "p0" } else { "p0.p0" };
                let name = format_in!(
                    self.alloc,
                    "llvm.{}.{ptrs}.{}",
                    op.name(),
                    view.ty(len).elem().name()
                );
                let value =
                    |v: Value| (LlType(view, view.ty(v)), format_in!(self.alloc, "%v{}", v.0));
                let flag = String::from_str_in(
                    if volatile.is_empty() { "false" } else { "true" },
                    self.alloc,
                );
                return self.intrinsic(
                    i,
                    name,
                    t,
                    &[value(dst), value(src), value(len), (LlType(view, Ty::I1), flag)],
                );
            }
            Op::Memcmp => {
                let (a, b, len) = (o.value(), o.value(), o.value());
                if view.ty(len).elem().bits(view.head.ptr_bits) != view.head.ptr_bits {
                    return Err(self.error("memcmp length differs from size_t"));
                }
                let name = String::from_str_in("memcmp", self.alloc);
                let value =
                    |v: Value| (LlType(view, view.ty(v)), format_in!(self.alloc, "%v{}", v.0));
                return self.intrinsic(i, name, t, &[value(a), value(b), value(len)]);
            }
            Op::AtomicLoad => format_into!(
                ir,
                "  %v{i} = load atomic{volatile} {t}, ptr %v{} {}, align {align}",
                o.value().0,
                order(flags.ordering())
            ),
            Op::AtomicStore => {
                let v = o.value();
                format_into!(
                    ir,
                    "  store atomic{volatile} {} %v{}, ptr %v{} {}, align {align}",
                    LlType(view, view.ty(v)),
                    v.0,
                    o.value().0,
                    order(flags.ordering())
                );
            }
            Op::AtomicRmw => {
                let op = o.rmw();
                let ptr = o.value();
                let val = o.value();
                format_into!(
                    ir,
                    "  %v{i} = atomicrmw{volatile} {} ptr %v{}, {t} %v{} {}, align {align}",
                    op.name(),
                    ptr.0,
                    val.0,
                    order(flags.ordering())
                );
            }
            Op::Cmpxchg => {
                let (ptr, expected, new) = (o.value(), o.value(), o.value());
                let ty = LlType(view, view.ty(expected));
                let weak = if flags.contains(Flags::WEAK) { " weak" } else { "" };
                format_into!(
                    ir,
                    "  %v{i} = cmpxchg{weak}{volatile} ptr %v{}, {ty} %v{}, {ty} %v{} {} {}, align {align}",
                    ptr.0,
                    expected.0,
                    new.0,
                    order(flags.ordering()),
                    order(flags.failure())
                );
            }
            Op::Fence => {
                let scope = if flags.contains(Flags::SINGLE_THREAD) {
                    " syncscope(\"singlethread\")"
                } else {
                    ""
                };
                format_into!(ir, "  fence{scope} {}", order(flags.ordering()));
            }
            _ => return Err(self.error(op.name())),
        }
        ir.push('\n');
        Ok(())
    }
}
