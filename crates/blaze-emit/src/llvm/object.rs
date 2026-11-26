//! The object file LLVM wrote for one prepared module, read back as a sealed `bzcode07` record.
//!
//! The function's code is its atom in `__text` (Mach-O has no COMDATs: LLVM marks every Mach-O
//! object `.subsections_via_symbols`, under which each symbol starts a piece of a section the
//! linker places on its own). Constants LLVM put in sections of their own (jump tables, float
//! literals) are laid out after it in the same bytes, so the record is one piece of code that
//! refers to nothing but what the function asked for. What the code says about where they are is
//! filled in here; what it says about the outside is a [`Site`] naming its target by
//! (space, symbol), which the lowering lane's `references` resolves from the symbol's name.
//!
//! Ported from the fork's `hir-ty-codegen/llvm/emitted.rs` (`ccb0b95cf7`), onto `CodeRecord`.


use blaze_code::record::{CodeRecord, Held, RecordError, Site, SiteKind, Space, Target, TrapRow};
use core::fmt;

use blaze_own::{AVec, Alloc, FixedString, String, Vec};
use object::{
    BinaryFormat, Object, ObjectSection, ObjectSymbol, RelocationFlags, RelocationTarget, SectionIndex, SectionKind, SymbolSection, elf, macho,
};

use super::LlvmModule;

/// Why an object file could not be read back as a record.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// `object` refused the file.
    Object,
    /// The function's symbol is not in the object.
    NoFunction,
    /// The function's symbol is not in a section.
    NoSection,
    /// The function's code is not where its section says.
    CodeOutside,
    /// A relocation names a symbol the lowering lane's `references` does not.
    UnknownSymbol { name: FixedString<80> },
    /// A relocation of a kind this reader does not map.
    Relocation { at: u64, kind: u32 },
    /// A direct `bl`/`b` to a callee. The record form's `Call` site is the 12-byte
    /// `adrp`+`ldr`+`blr` sequence the loader rewrites (`blaze_code::image`), which a direct
    /// branch is not: written as a `Call` it would run past the function and the loader would
    /// refuse it. Refused by name until the lowering emits the form's sequence.
    DirectCall { at: u64 },
    /// A relocation against a section the code does not lay out.
    Section { at: u64 },
    /// A `ldr` whose `adrp` is not the instruction before it naming the same target: the
    /// record form's `Load` site is the adjacent pair, which LLVM did not emit.
    UnpairedLoad { at: u64, target: Target },
    /// A `brk` for an `unreachable` whose `TrapSource` the module does not carry.
    TrapSource { at: u32 },
    /// A constant section holds an address of the code, which a record cannot name.
    AddressOfCode { at: u64 },
    /// A section the code refers to is writable.
    Writable { section: FixedString<32> },
    /// The record did not seal.
    Seal(RecordError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Object => write!(f, "not an object file"),
            ReadError::NoFunction => write!(f, "the function's symbol is not in the object"),
            ReadError::NoSection => write!(f, "the function's symbol is in no section"),
            ReadError::CodeOutside => write!(f, "the function's code is not where its section says"),
            ReadError::UnknownSymbol { name } => write!(f, "a relocation names {name}, which the module does not"),
            ReadError::Relocation { at, kind } => write!(f, "a relocation of kind {kind} at {at}"),
            ReadError::DirectCall { at } => write!(f, "a direct call at {at}: the record form's call is the 12-byte `adrp`+`ldr`+`blr` the loader rewrites, which a direct branch is not"),
            ReadError::Section { at } => write!(f, "a relocation at {at} against a section the code does not lay out"),
            ReadError::UnpairedLoad { at, target } => write!(f, "a load at {at} of {target:?} whose `adrp` is not the instruction before it"),
            ReadError::TrapSource { at } => write!(f, "a `brk` at {at} for an `unreachable` whose source the module does not carry"),
            ReadError::AddressOfCode { at } => write!(f, "a constant at {at} holds an address of the code"),
            ReadError::Writable { section } => write!(f, "the code refers to the writable section {section}"),
            ReadError::Seal(why) => write!(f, "the record did not seal: {why}"),
        }
    }
}

impl core::error::Error for ReadError {}

/// The symbol's name as the lowering lane spells it: `blaze_fn_<64 hex>`, without the `@`.
/// `definition_name` is the range of `@blaze_fn_<hex>` in the IR, `@` included, so the `@`
/// is taken off here: an object's symbols carry no `@`.
fn function_name<'a>(module: &LlvmModule<'_>, alloc: Alloc<'a>) -> String<Alloc<'a>> {
    let range = module.definition_name.clone();
    let spelled = core::str::from_utf8(&module.ir[range]).unwrap_or("");
    String::from_str_in(spelled.strip_prefix('@').unwrap_or(spelled), alloc)
}

/// Where a relocation leads: a named symbol (an import, or the function's own) or a section.
enum Destination<'name> {
    Named(&'name str),
    Section,
}

/// Reads `object` (what LLVM wrote for `module`) as a sealed `bzcode07` record in `alloc`.
pub fn read<'a>(object: &[u8], module: &LlvmModule<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ReadError> {
    let file = object::File::parse(object).map_err(|_| ReadError::Object)?;
    let name = function_name(module, alloc);
    let macho = file.format() == BinaryFormat::MachO;
    let mut found: Option<(SectionIndex, u64)> = None;
    for symbol in file.symbols() {
        let (Ok(symbol_name), SymbolSection::Section(section)) = (symbol.name(), symbol.section()) else {
            continue;
        };
        if plain(macho, symbol_name) == name {
            found = Some((section, symbol.address()));
        }
    }
    let (text, address) = found.ok_or(ReadError::NoFunction)?;
    let section = file.section_by_index(text).map_err(|_| ReadError::NoSection)?;
    let range = atom(&file, text, address)?;
    let code = section.data().map_err(|_| ReadError::CodeOutside)?.get(range.start as usize..range.end as usize).ok_or(ReadError::CodeOutside)?;

    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(code);
    // The code's alignment, at least the record form's least: Mach-O's `__text` reports a
    // 4-byte alignment, and the form asks 16. LLVM aligns every AArch64 function to 16, so
    // the record's claim is true of the code it holds.
    record.code_align = align_of(section.align()).max(blaze_code::record::LEAST_CODE_ALIGN);
    record.relro_align = blaze_code::record::LEAST_RELRO_ALIGN;
    // The frame word: LLVM's own prologue sizes it, which the record's form does not read
    // (the SOTA tier's `frame` is its pass's); a release record keeps the convention of the
    // tier it stands in for, so the launcher's trap reporting finds the frame.
    record.frame = 0;

    // Every section the code's relocations lead to, laid out after it.
    let mut placed: Vec<(SectionIndex, u64), Alloc<'a>> = Vec::new_in(alloc);
    let mut pending: Vec<SectionIndex, Alloc<'a>> = Vec::from_slice_in(&[text], alloc);
    while let Some(index) = pending.pop() {
        if placed.iter().any(|&(at, _)| at == index) {
            continue;
        }
        let section = file.section_by_index(index).map_err(|_| ReadError::Section { at: 0 })?;
        if index != text {
            match section.kind() {
                SectionKind::Text | SectionKind::ReadOnlyData | SectionKind::ReadOnlyString | SectionKind::ReadOnlyDataWithRel => {}
                SectionKind::Data if section.name().map(|n| n == "__const").unwrap_or(false) => {}
                _ => {
                    let mut spelled = FixedString::<32>::new();
                    spelled.try_push_str(section.name().unwrap_or("")).ok();
                    return Err(ReadError::Writable { section: spelled });
                }
            }
            let align = section.align().max(1);
            let start = (record.code.len() as u64).next_multiple_of(align);
            record.code.resize(start as usize, 0);
            record.code.extend_from_slice(section.data().map_err(|_| ReadError::CodeOutside)?);
            record.code_align = record.code_align.max(align_of(align));
            placed.push((index, start));
        } else {
            placed.push((index, 0));
        }
        for (_, relocation) in section.relocations() {
            if let RelocationTarget::Section(target) = relocation.target() {
                pending.push(target);
            }
        }
    }

    // The relocations of the code. A `bl` is a `Call` site on its own. An `adrp` is the
    // first half of a `Load` site and its `ldr` the second: the record form names the pair as
    // one 8-byte site, so the two are paired by the symbol they name (LLVM may put an
    // unrelated instruction between them, which the form cannot express: refused by name,
    // never written as a wrong site). A `PAGEOFF12` whose `adrp` is not in this function's
    // code is the second half of a site the caller owns, and is left alone.
    let section = file.section_by_index(text).map_err(|_| ReadError::NoSection)?;
    let mut pages: Vec<(u64, Target), Alloc<'a>> = Vec::new_in(alloc);
    for (offset, relocation) in section.relocations() {
        if !range.contains(&offset) {
            continue;
        }
        let at = offset - range.start;
        let is_page = matches!(relocation.flags(), RelocationFlags::MachO { r_type: macho::ARM64_RELOC_PAGE21 | macho::ARM64_RELOC_GOT_LOAD_PAGE21, .. }
            | RelocationFlags::Elf { r_type: elf::R_AARCH64_ADR_PREL_PG_HI21 | elf::R_AARCH64_ADR_GOT_PAGE });
        if !is_page {
            continue;
        }
        let target = named_target(&file, &relocation, at, module, alloc)?;
        pages.push((at, target));
    }
    for (offset, relocation) in section.relocations() {
        if !range.contains(&offset) {
            continue;
        }
        let at = offset - range.start;
        let flags = relocation.flags();
        match flags {
            RelocationFlags::MachO { r_type: macho::ARM64_RELOC_BRANCH26, .. }
            | RelocationFlags::Elf { r_type: elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 } => {
                return Err(ReadError::DirectCall { at });
            }
            RelocationFlags::MachO { r_type: macho::ARM64_RELOC_PAGE21 | macho::ARM64_RELOC_GOT_LOAD_PAGE21, .. }
            | RelocationFlags::Elf { r_type: elf::R_AARCH64_ADR_PREL_PG_HI21 | elf::R_AARCH64_ADR_GOT_PAGE } => {
                // The `adrp`: its `ldr` is found when the `PAGEOFF12` is read.
                continue;
            }
            RelocationFlags::MachO { r_type: macho::ARM64_RELOC_PAGEOFF12 | macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12, .. }
            | RelocationFlags::Elf { r_type: elf::R_AARCH64_ADD_ABS_LO12_NC | elf::R_AARCH64_LDST8_ABS_LO12_NC | elf::R_AARCH64_LDST16_ABS_LO12_NC | elf::R_AARCH64_LDST32_ABS_LO12_NC | elf::R_AARCH64_LDST64_ABS_LO12_NC | elf::R_AARCH64_LDST128_ABS_LO12_NC | elf::R_AARCH64_LD64_GOT_LO12_NC } => {
                let target = named_target(&file, &relocation, at, module, alloc)?;
                if !pages.iter().any(|&(page_at, page_target)| page_at + 4 == at && page_target == target) {
                    return Err(ReadError::UnpairedLoad { at, target });
                }
                let index = record.target(target);
                record.sites.push(Site { at: (at - 4) as u32, kind: SiteKind::Load, target: index, addend: 0 });
                continue;
            }
            RelocationFlags::MachO { r_type, .. } => return Err(ReadError::Relocation { at, kind: u32::from(r_type.0) }),
            RelocationFlags::Elf { r_type } => return Err(ReadError::Relocation { at, kind: r_type.0 }),
            _ => return Err(ReadError::Relocation { at, kind: 0 }),
        }
    }

    // The traps: each `brk` in LLVM's code is one the lowerer wrote for a LIR `trap` or
    // `unreachable`. An `unreachable`'s `brk #0xff02` takes the next `TrapSource` by ordinal
    // (the build's rows, in LIR order); a `trap`'s `brk #code` names its own code and has no
    // source row. LLVM may drop an `unreachable` it proves dead, so a source left over is not
    // an error; one missing when a `brk` needs it is.
    let mut next_source = 0usize;
    for at in (0..code.len()).step_by(4) {
        let Some(word) = code.get(at..at + 4).and_then(|w| w.try_into().ok()) else { break };
        let word = u32::from_le_bytes(word);
        if word & 0xffe0_001f != 0xd420_0000 {
            continue;
        }
        let immediate = (word >> 5) & 0xffff;
        if immediate != blaze_code::trap::UNREACHABLE {
            continue;
        }
        let Some(source) = module.traps.get(next_source) else {
            return Err(ReadError::TrapSource { at: at as u32 });
        };
        next_source += 1;
        record.traps.push(TrapRow { at: at as u32, kind: source.kind, site: source.site, held: Held::Unknown, subject: source.subject });
    }

    let sealed = record.seal().map_err(ReadError::Seal)?;
    let mut out: AVec<'a, u8> = Vec::new_in(alloc);
    sealed.encode(&mut out);
    Ok(out)
}

/// Where in `text`, a section of a Mach-O object, the code of the function whose symbol is at
/// `address` is: up to the next symbol there, or the end of the section.
fn atom(file: &object::File<'_>, text: SectionIndex, address: u64) -> Result<core::ops::Range<u64>, ReadError> {
    let section = file.section_by_index(text).map_err(|_| ReadError::NoSection)?;
    let next = file
        .symbols()
        .filter(|it| it.section() == SymbolSection::Section(text) && it.address() > address)
        .filter(|it| it.name().is_ok_and(|name| !name.starts_with(['l', 'L'])))
        .map(|it| it.address())
        .min();
    let end = next.unwrap_or(section.address() + section.size());
    let start = address.checked_sub(section.address()).ok_or(ReadError::CodeOutside)?;
    Ok(start..end - section.address())
}

/// The `Target` a relocation names, as the lowering lane's `references` spell it; a
/// relocation against a section (the function's own constants) names none.
fn named_target<'a>(file: &object::File<'_>, relocation: &object::Relocation, at: u64, module: &LlvmModule<'_>, alloc: Alloc<'a>) -> Result<Target, ReadError> {
    let RelocationTarget::Symbol(index) = relocation.target() else {
        return Err(ReadError::Section { at });
    };
    let symbol = file.symbol_by_index(index).map_err(|_| ReadError::Object)?;
    match destination(file, &symbol)? {
        Destination::Named(name) => target_of(module, name, alloc),
        Destination::Section => Err(ReadError::Section { at }),
    }
}

/// Where a relocation leads.
fn destination<'file>(file: &'file object::File<'_>, symbol: &object::Symbol<'_, 'file>) -> Result<Destination<'file>, ReadError> {
    let macho = file.format() == BinaryFormat::MachO;
    if !matches!(symbol.section(), SymbolSection::Section(_)) || symbol.is_global() {
        return Ok(Destination::Named(plain(macho, symbol.name().map_err(|_| ReadError::Object)?)));
    }
    Ok(Destination::Section)
}

/// A symbol's name as the lowering lane spells it: Mach-O keeps a leading `_` on every
/// symbol, which LLVM adds and `object` does not take off.
fn plain<'n>(macho: bool, name: &'n str) -> &'n str {
    if macho { name.strip_prefix('_').unwrap_or(name) } else { name }
}

/// The `Target` of the symbol `name`, as the lowering lane's `references` names it.
fn target_of(module: &LlvmModule<'_>, name: &str, alloc: Alloc<'_>) -> Result<Target, ReadError> {
    let Some(target) = module.references.iter().flatten().find(|t| LinkName(**t).spells(name, alloc)) else {
        let mut spelled = FixedString::<80>::new();
        spelled.try_push_str(name).ok();
        return Err(ReadError::UnknownSymbol { name: spelled });
    };
    Ok(*target)
}

/// log2 of an alignment, as the record's `code_align` is.
fn align_of(align: u64) -> u8 {
    u8::try_from(align.max(1).trailing_zeros()).unwrap_or(blaze_code::record::MOST_PART_ALIGN)
}

/// How the lowering lane spells a target's link name (its `LinkName`, which is private).
struct LinkName(Target);

impl LinkName {
    /// Whether the lowering lane spells this target `name`.
    fn spells(self, name: &str, alloc: Alloc<'_>) -> bool {
        let prefix = match self.0.space {
            Space::Func => "blaze_fn_",
            Space::Static => "blaze_static_",
            Space::Vtable => "blaze_vtable_",
            Space::Tls => "blaze_tls_",
            Space::Outside => return false,
        };
        let mut spelled = String::new_in(alloc);
        spelled.push_str(prefix);
        for b in self.0.symbol.0 {
            blaze_own::format_into!(&mut spelled, "{b:02x}");
        }
        &*spelled == name
    }
}
