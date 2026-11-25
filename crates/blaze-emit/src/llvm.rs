//! Verified, ABI-lowered LIR to a relocatable single-definition LLVM module.
//! The same assembly is the input to release optimization and constant-masked folding.

use crate::{Named, Names, Outside};
use blaze_code::record::{Space, Symbol, Target};
use blaze_lir::{
    Block, Op, Ty, Value, Verified, View,
    func::{RefId, RefKind, SigId},
};
use blaze_own::{AString, AVec, Alloc, BTreeMap, String, Vec, format_in, format_into};
use core::{fmt, ops::Range};

mod abi;
mod control;
mod instruction;
mod memory;
#[cfg(test)]
mod tests;
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
    /// One entry per LIR ref, including local data, whose identity is inside this module.
    pub references: AVec<'a, Option<Target>>,
    /// Rust direct callees in first-use order, excluding recursive calls.
    pub calls: AVec<'a, Symbol>,
    pub literals: AVec<'a, Literal>,
    /// Placement patches these bytes in the named `blaze_pos_blaze_fn_<hex>_<row>`
    /// data symbol. Data with position fields is opaque to LLVM load folding.
    pub positions: AVec<'a, (u32, blaze_lir::func::DataPositionRec)>,
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
    names: AVec<'a, AString<'a>>,
    calls: AVec<'a, Symbol>,
    literals: AVec<'a, Literal>,
    declarations: blaze_own::ABTreeMap<'a, AString<'a>, AString<'a>>,
    edges: control::Edges<'a, 'v>,
    at: Option<usize>,
}

impl<'a> Lower<'a, '_> {
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
        ir: String::new_in(alloc),
        names: Vec::new_in(alloc),
        references: Vec::new_in(alloc),
        calls: Vec::new_in(alloc),
        literals: Vec::new_in(alloc),
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
        let (target, name) = match names.name(outside) {
            Some(Named::Symbol(symbol)) => {
                let target = Target { space, symbol };
                (target, format_in!(alloc, "@{}", LinkName(target)))
            }
            Some(Named::C(name)) => {
                if name.is_empty() || name.as_bytes().contains(&0) {
                    return Err(lower.error("empty or NUL-containing C link name"));
                }
                let mut quoted = String::from_str_in("@\"", alloc);
                escape(&mut quoted, name.as_bytes());
                quoted.push('"');
                (Target { space: Space::Outside, symbol: Symbol::of_name(name) }, quoted)
            }
            None => return Err(lower.error("unresolved outside reference in verified LIR")),
        };
        lower.references.push(Some(target));
        lower.names.push(name);
    }
    let own = Signature::new(view, SigId(view.head.sig)).map_err(|e| lower.error(e))?;
    let llvm_name = format_in!(alloc, "@{}", LinkName(Target { space: Space::Func, symbol }));
    format_into!(&mut lower.ir, "define {} ", own.prefix());
    let start = lower.ir.len();
    lower.ir.push_str(&llvm_name);
    let definition_name = start..lower.ir.len();
    own.parameters(&mut lower.ir, abi::Args::Definition);
    abi::attributes(&mut lower.ir, view.head.attrs);
    if !view.features.is_empty() {
        lower.ir.push_str(" \"target-features\"=\"");
        escape(&mut lower.ir, view.features);
        lower.ir.push('"');
    }
    lower.ir.push_str(" uwtable");
    if (0..view.blocks()).any(|b| view.block_kind(Block(b as u32)).is_landing()) {
        lower.ir.push_str(" personality ptr @rust_eh_personality");
        lower.declaration(
            String::from_str_in("@rust_eh_personality", alloc),
            String::from_str_in("declare i32 @rust_eh_personality(...)", alloc),
        )?;
    }
    lower.ir.push_str(" {\n");
    for b in 0..view.blocks() {
        let block = Block(b as u32);
        format_into!(&mut lower.ir, "b{b}:\n");
        lower.block_params(block)?;
        if b == 0 {
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
    let mut positions = Vec::with_capacity_in(view.data_positions.len(), alloc);
    for k in 0..view.datas.len() {
        for &p in view.data_positions_of(blaze_lir::func::DataId(k as u32)) {
            positions.push((k as u32, p));
        }
    }
    Ok(LlvmModule {
        symbol,
        ir: lower.ir.into_bytes(),
        definition_name,
        references: lower.references,
        positions,
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
