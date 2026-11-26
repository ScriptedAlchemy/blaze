//! Verified, ABI-lowered LIR to a relocatable single-definition LLVM module.
//! The same assembly is the input to release optimization and constant-masked folding.

use crate::{Named, Names, Outside};
use blaze_code::record::{Space, Symbol, Target};
use blaze_lir::{
    Block, Op, Ty, Value, Verified, View,
    func::{RefId, RefKind, SigId},
};
use blaze_own::{AString, AVec, Alloc, BTreeMap, format_in, format_into};
use core::{fmt, ops::Range};

mod abi;
mod control;
pub mod folding;
mod instruction;
mod memory;
#[cfg(test)]
mod tests;
mod tls;
use abi::{LlType, Signature};

/// The byte range is the LLVM operand of the constant-producing instruction. Float
/// operands are bitcast from integers so every NaN payload and signed zero survives.
#[derive(Debug)]
pub struct Literal {
    pub ty: Ty,
    pub bits: u128,
    pub operand: Range<usize>,
}

pub struct LlvmModule<'a> {
    pub symbol: Symbol,
    pub ir: AVec<'a, u8>,
    pub definition_name: Range<usize>,
    /// The original ABI and the parameter insertion point of a forwardable definition.
    pub fold_abi: Option<folding::Abi<'a>>,
    /// One entry per LIR ref, including local data, whose identity is inside this module.
    pub references: AVec<'a, Option<Target>>,
    /// Additional targets introduced by lowering, in first-use order (indexed TLS and libc).
    pub implicit_references: AVec<'a, Target>,
    /// Rust direct callees in first-use order, excluding recursive calls.
    pub calls: AVec<'a, Symbol>,
    pub literals: AVec<'a, Literal>,
    /// Placement patches these bytes in the named `blaze_pos_blaze_fn_<hex>_<row>`
    /// data symbol. Data with position fields is opaque to LLVM load folding.
    pub positions: AVec<'a, (u32, blaze_lir::func::DataPositionRec)>,
    /// The source build's item-relative trap metadata, by its original LIR trap ordinal.
    pub traps: AVec<'a, blaze_code::record::TrapSource>,
    pub instructions: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct LowerError {
    pub symbol: Symbol,
    pub instruction: Option<usize>,
    pub reason: &'static str,
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LLVM lowering of {}", self.symbol)?;
        if let Some(i) = self.instruction {
            write!(f, " at LIR instruction {i}")?;
        }
        write!(f, ": {}", self.reason)
    }
}
impl core::error::Error for LowerError {}

pub struct LinkName(pub Target);

impl fmt::Display for LinkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.0.space {
            Space::Func => "blaze_fn_",
            Space::Static => "blaze_static_",
            Space::Vtable => "blaze_vtable_",
            Space::Tls => "blaze_tls_",
            Space::Outside => return Err(fmt::Error),
        };
        f.write_str(prefix)?;
        for b in self.0.symbol.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

struct Lower<'a, 'v> {
    alloc: Alloc<'a>,
    view: View<'v>,
    symbol: Symbol,
    ir: AString<'a>,
    references: AVec<'a, Option<Target>>,
    implicit_references: AVec<'a, Target>,
    resolver: &'v dyn Names,
    names: AVec<'a, AString<'a>>,
    calls: AVec<'a, Symbol>,
    literals: AVec<'a, Literal>,
    declarations: blaze_own::ABTreeMap<'a, AString<'a>, AString<'a>>,
    edges: control::Edges<'a, 'v>,
    at: Option<usize>,
}

impl<'a> Lower<'a, '_> {
    fn implicit(&mut self, target: Target) {
        if !self.implicit_references.contains(&target) {
            self.implicit_references.push(target);
        }
    }

    fn link(&self, outside: Outside, space: Space) -> Result<(Target, AString<'a>), LowerError> {
        match self.resolver.name(outside) {
            Some(Named::Symbol(symbol)) => {
                let target = Target { space, symbol };
                Ok((target, format_in!(self.alloc, "@{}", LinkName(target))))
            }
            Some(Named::C(name)) => {
                if name.is_empty() || name.as_bytes().contains(&0) {
                    return Err(self.error("empty or NUL-containing C link name"));
                }
                if !matches!(space, Space::Func | Space::Static) {
                    return Err(self.error("a C name for a vtable or thread-local"));
                }
                Ok((
                    Target { space: Space::Outside, symbol: Symbol::of_name(name) },
                    external(name, self.alloc),
                ))
            }
            None => Err(self.error("unresolved outside reference in verified LIR")),
        }
    }
    fn result_type(&self, i: usize) -> Result<Ty, LowerError> {
        self.view
            .tys
            .get(i)
            .copied()
            .map(Ty)
            .ok_or_else(|| self.error("instruction missing its LIR type"))
    }
    fn ref_name(&self, id: RefId) -> Result<&str, LowerError> {
        self.names
            .get(id.index())
            .map(AString::as_str)
            .ok_or_else(|| self.error("missing LLVM link name"))
    }
    fn error(&self, reason: &'static str) -> LowerError {
        LowerError { symbol: self.symbol, instruction: self.at, reason }
    }
    fn declaration(
        &mut self,
        name: AString<'a>,
        declaration: AString<'a>,
    ) -> Result<(), LowerError> {
        if let Some(previous) = self.declarations.get(&name) {
            if previous != &declaration {
                return Err(self.error("one external name has conflicting LLVM declarations"));
            }
        } else {
            self.declarations.insert(name, declaration);
        }
        Ok(())
    }
}

pub fn prepare<'a>(
    symbol: Symbol,
    body: Verified<'_>,
    names: &dyn Names,
    alloc: Alloc<'a>,
) -> Result<LlvmModule<'a>, LowerError> {
    let view = body.view();
    let mut lower = Lower {
        alloc,
        symbol,
        view,
        ir: AString::new_in(alloc),
        names: AVec::new_in(alloc),
        references: AVec::new_in(alloc),
        implicit_references: AVec::new_in(alloc),
        resolver: names,
        calls: AVec::new_in(alloc),
        literals: AVec::new_in(alloc),
        declarations: BTreeMap::new_in(alloc),
        edges: control::Edges::new(view, alloc),
        at: None,
    };
    for row in view.refs {
        let (outside, space) = match RefKind::from_word(row.kind) {
            Some(RefKind::Func) => (Outside::Func(row.place), Space::Func),
            Some(RefKind::Static) => (Outside::Static(row.place), Space::Static),
            Some(RefKind::Vtable) => (Outside::Vtable(row.place), Space::Vtable),
            Some(RefKind::Tls) => (Outside::Tls(row.place), Space::Tls),
            Some(RefKind::Data) => {
                lower.references.push(None);
                lower.names.push(format_in!(alloc, "@d{}", row.place));
                continue;
            }
            None => return Err(lower.error("unknown LIR reference kind")),
        };
        let (target, name) = lower.link(outside, space)?;
        lower.references.push(Some(target));
        lower.names.push(name);
    }
    let own = Signature::new(view, SigId(view.head.sig)).map_err(|e| lower.error(e.reason()))?;
    let llvm_name = format_in!(alloc, "@{}", LinkName(Target { space: Space::Func, symbol }));
    format_into!(&mut lower.ir, "define {} ", own.prefix());
    let start = lower.ir.len();
    lower.ir.push_str(&llvm_name);
    let definition_name = start..lower.ir.len();
    own.parameters(&mut lower.ir, abi::Args::Definition);
    let end_parameters = lower.ir.len();
    abi::attributes(&mut lower.ir, view.head.attrs);
    if !view.features.is_empty() {
        lower.ir.push_str(" \"target-features\"=\"");
        escape(&mut lower.ir, view.features);
        lower.ir.push('"');
    }
    lower.ir.push_str(" uwtable");
    if view
        .block_kinds
        .iter()
        .filter_map(|&k| blaze_lir::BlockKind::from_byte(k))
        .any(|k| k.is_landing())
    {
        lower.ir.push_str(" personality ptr @rust_eh_personality");
        lower.declaration(
            AString::from_str_in("@rust_eh_personality", alloc),
            AString::from_str_in("declare i32 @rust_eh_personality(...)", alloc),
        )?;
    }
    lower.ir.push_str(" {\n");
    let end_header = lower.ir.len();
    for (b, _) in (0u32..).zip(view.block_starts) {
        let block = Block(b);
        format_into!(&mut lower.ir, "b{b}:\n");
        lower.block_params(block)?;
        if b == 0 {
            lower.copy_slots()?;
            for (i, slot) in view.slots.iter().enumerate() {
                let align = 1u64
                    .checked_shl(slot.align.into())
                    .ok_or_else(|| lower.error("stack alignment exceeds LLVM limit"))?;
                format_into!(
                    &mut lower.ir,
                    "  %s{i} = alloca i8, i32 {}, align {align}\n",
                    slot.size
                );
            }
        }
        for i in view.params(block).end..view.block(block).end {
            lower.at = Some(i);
            lower.instruction(i)?;
        }
    }
    lower.edges.write(&mut lower.ir);
    lower.ir.push_str("}\n");
    lower.at = None;
    lower.globals(&llvm_name)?;
    for (_, decl) in &lower.declarations {
        lower.ir.push_str(decl);
        lower.ir.push('\n');
    }
    let mut positions = AVec::with_capacity_in(view.data_positions.len(), alloc);
    for (k, _) in (0u32..).zip(view.datas) {
        for &p in view.data_positions_of(blaze_lir::func::DataId(k)) {
            positions.push((k, p));
        }
    }
    let fold_abi = folding::Abi::of(&lower, own, end_parameters, end_header);
    Ok(LlvmModule {
        symbol,
        ir: lower.ir.into_bytes(),
        definition_name,
        fold_abi,
        references: lower.references,
        implicit_references: lower.implicit_references,
        positions,
        traps: AVec::new_in(alloc),
        calls: lower.calls,
        literals: lower.literals,
        instructions: view.len(),
    })
}

fn escape(out: &mut AString<'_>, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            b' '..=b'~' if byte != b'"' && byte != b'\\' => out.push(char::from(byte)),
            _ => format_into!(out, "\\{byte:02X}"),
        }
    }
}

fn external<'a>(name: &str, alloc: Alloc<'a>) -> AString<'a> {
    // Use a single spelling for libc names also emitted by LIR operations such as memcmp.
    if name.bytes().next().is_some_and(|b| !b.is_ascii_digit())
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-.$_".contains(&c))
    {
        return format_in!(alloc, "@{name}");
    }
    let mut quoted = AString::from_str_in("@\"", alloc);
    escape(&mut quoted, name.as_bytes());
    quoted.push('"');
    quoted
}
