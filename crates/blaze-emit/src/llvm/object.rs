//! The object file LLVM wrote for one prepared module, read back as a sealed code record.
//!
//! The function's code is its atom in `__text` (Mach-O has no COMDATs: LLVM marks every Mach-O
//! object `.subsections_via_symbols`, under which each symbol starts a piece of a section the
//! linker places on its own). Constants LLVM put in sections of their own (jump tables, float
//! literals) are laid out in its relro part, so the record is one piece of code that
//! refers to nothing but what the function asked for. What the code says about where they are is
//! filled in here; what it says about the outside is a [`Site`] naming its target by
//! (space, symbol), which the lowering lane's `references` resolves from the symbol's name.
//!
//! Ported from the fork's `hir-ty-codegen/llvm/emitted.rs` (`ccb0b95cf7`), onto `CodeRecord`.


use blaze_code::record::{CodeRecord, Held, RecordError, Site, SiteKind, Space, Target, TrapRow, Word, PositionRow, PositionPart, PositionField, OWN_CODE, OWN_RELRO};
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
    /// A recognized relocation whose operand or target cannot have the required meaning.
    Semantics { at: u64, what: &'static str },
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
            ReadError::Section { at } => write!(f, "a relocation at {at} against a section the code does not lay out"),
            ReadError::UnpairedLoad { at, target } => write!(f, "a load at {at} of {target:?} whose `adrp` is not the instruction before it"),
            ReadError::TrapSource { at } => write!(f, "a `brk` at {at} for an `unreachable` whose source the module does not carry"),
            ReadError::AddressOfCode { at } => write!(f, "a constant at {at} holds an address of the code"),
            ReadError::Writable { section } => write!(f, "the code refers to the writable section {section}"),
            ReadError::Semantics { at, what } => write!(f, "native object relocation at {at}: {what}"),
            ReadError::Seal(why) => write!(f, "the record did not seal: {why}"),
        }
    }
}

impl core::error::Error for ReadError {}

/// The symbol's name as the lowering lane spells it: `blaze_fn_<64 hex>`, without the `@`.
fn function_name<'a>(module: &LlvmModule<'_>, alloc: Alloc<'a>) -> Result<String<Alloc<'a>>, ReadError> {
    let range = module.definition_name.clone();
    let name = core::str::from_utf8(module.ir.get(range).ok_or(ReadError::NoFunction)?).map_err(|_| ReadError::NoFunction)?;
    Ok(String::from_str_in(name.strip_prefix('@').unwrap_or(name), alloc))
}

/// Where a relocation leads: a named symbol (an import, or the function's own) or a section.
enum Destination {
    Named(Target),
    Section { index: SectionIndex, offset: i64 },
}

/// Reads `object` (what LLVM wrote for `module`) as a sealed code record in `alloc`.
pub fn read<'a>(object: &[u8], module: &LlvmModule<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ReadError> {
    let file = object::File::parse(object).map_err(|_| ReadError::Object)?;
    if file.architecture() != object::Architecture::Aarch64 || !file.is_little_endian() {
        return Err(ReadError::Semantics { at: 0, what: "only little-endian AArch64 object code is supported" });
    }
    let name = function_name(module, alloc)?;
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
    let start = usize::try_from(range.start).map_err(|_| ReadError::CodeOutside)?;
    let end = usize::try_from(range.end).map_err(|_| ReadError::CodeOutside)?;
    let code = section.data().map_err(|_| ReadError::CodeOutside)?.get(start..end).ok_or(ReadError::CodeOutside)?;

    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(code);
    record.code_align = align_of(section.align()).max(blaze_code::record::LEAST_CODE_ALIGN);
    record.relro_align = blaze_code::record::LEAST_RELRO_ALIGN;
    // The frame word: LLVM's own prologue sizes it, which the record's form does not read
    // (the SOTA tier's `frame` is its pass's); a release record keeps the convention of the
    // tier it stands in for, so the launcher's trap reporting finds the frame.
    record.frame = 0;
    for symbol in file.symbols().filter(|symbol| symbol.is_undefined()) {
        let symbol_name = plain(macho, symbol.name().map_err(|_| ReadError::Object)?);
        if module.references.iter().flatten().chain(module.implicit_references.iter()).any(|target| target.space == Space::Outside && target.symbol == blaze_code::record::Symbol::of_name(symbol_name)) {
            record.outside(symbol_name);
        }
    }

    // Reach every local data section through both instruction and data relocations. Data
    // stays in relro: absolute words and source positions are filled only at placement.
    let mut placed: Vec<(SectionIndex, u64), Alloc<'a>> = Vec::new_in(alloc);
    let mut pending: Vec<SectionIndex, Alloc<'a>> = Vec::from_slice_in(&[text], alloc);
    while let Some(index) = pending.pop() {
        if placed.iter().any(|&(at, _)| at == index) {
            continue;
        }
        let section = file.section_by_index(index).map_err(|_| ReadError::Section { at: 0 })?;
        if index != text {
            let kind = section.kind();
            match kind {
                SectionKind::ReadOnlyData | SectionKind::ReadOnlyString | SectionKind::ReadOnlyDataWithRel => {}
                SectionKind::Data if section.name().map(|n| n == "__const").unwrap_or(false) => {}
                SectionKind::Data | SectionKind::UninitializedData if position_section(&file, index, module, alloc)? => {}
                _ => {
                    let mut spelled = FixedString::<32>::new();
                    spelled.try_push_str(section.name().unwrap_or("")).ok();
                    return Err(ReadError::Writable { section: spelled });
                }
            }
            let align = section.align().max(1);
            if !align.is_power_of_two() || align > (1 << blaze_code::record::MOST_PART_ALIGN) {
                return Err(ReadError::Semantics { at: 0, what: "constant section alignment exceeds the record form" });
            }
            let start = u64::try_from(record.relro.len()).map_err(|_| ReadError::CodeOutside)?.next_multiple_of(align);
            let start_at = usize::try_from(start).map_err(|_| ReadError::CodeOutside)?;
            if kind == SectionKind::UninitializedData {
                // LLVM puts zero source-position slots in BSS, whose object section has no bytes.
                let size = usize::try_from(section.size()).map_err(|_| ReadError::CodeOutside)?;
                let end = start_at.checked_add(size).filter(|&end| u32::try_from(end).is_ok()).ok_or(ReadError::CodeOutside)?;
                record.relro.resize(end, 0);
            } else {
                record.relro.resize(start_at, 0);
                record.relro.extend_from_slice(section.data().map_err(|_| ReadError::CodeOutside)?);
            }
            record.relro_align = record.relro_align.max(align_of(align));
            placed.push((index, start));
        } else {
            placed.push((index, 0));
        }
        for (at, relocation) in section.relocations() {
            if index == text && !range.contains(&at) { continue; }
            if let Destination::Section { index: target, .. } = destination(&file, &relocation, at, module, alloc)? {
                pending.push(target);
            }
        }
    }
    for &(index, base) in &placed {
        let section = file.section_by_index(index).map_err(|_| ReadError::NoSection)?;
        for (offset, relocation) in section.relocations() {
            if index == text && !range.contains(&offset) { continue; }
            let dest = destination(&file, &relocation, offset, module, alloc)?;
            if index == text {
                instruction(&file, &relocation, offset - range.start, dest, &placed, text, range.start, &mut record)?;
            } else {
                data_relocation(&file, &relocation, base + offset, dest, &placed, text, range.start, &mut record)?;
            }
        }
    }
    positions(&file, module, &placed, &mut record, alloc)?;

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
        let at = u32::try_from(at).map_err(|_| ReadError::CodeOutside)?;
        let Some(source) = module.traps.get(next_source) else {
            return Err(ReadError::TrapSource { at });
        };
        next_source += 1;
        record.traps.push(TrapRow { at, kind: source.kind, site: source.site, held: Held::Unknown, subject: source.subject });
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
    if file.format() == BinaryFormat::Elf {
        if let Some(size) = file.symbols().find(|symbol| symbol.section() == SymbolSection::Section(text) && symbol.address() == address && symbol.kind() == object::SymbolKind::Text && symbol.size() != 0).map(|symbol| symbol.size()) {
            let start = address.checked_sub(section.address()).ok_or(ReadError::CodeOutside)?;
            let end = start.checked_add(size).filter(|end| *end <= section.size()).ok_or(ReadError::CodeOutside)?;
            return Ok(start..end);
        }
    }
    let next = file
        .symbols()
        .filter(|it| it.section() == SymbolSection::Section(text) && it.address() > address)
        .filter(|it| it.kind() == object::SymbolKind::Text)
        .filter(|it| it.name().is_ok_and(|name| !name.starts_with(['l', 'L'])))
        .map(|it| it.address())
        .min();
    let end = next.unwrap_or(section.address() + section.size());
    let start = address.checked_sub(section.address()).ok_or(ReadError::CodeOutside)?;
    Ok(start..end - section.address())
}

/// A defined symbol carries its offset even if it is global. Undefined symbols alone name
/// other records. This also makes recursive calls refer to this atom, not an absent import.
fn destination(file: &object::File<'_>, relocation: &object::Relocation, at: u64, module: &LlvmModule<'_>, alloc: Alloc<'_>) -> Result<Destination, ReadError> {
    match relocation.target() {
        RelocationTarget::Section(index) => Ok(Destination::Section { index, offset: 0 }),
        RelocationTarget::Symbol(index) => {
            let symbol = file.symbol_by_index(index).map_err(|_| ReadError::Object)?;
            let name = plain(file.format() == BinaryFormat::MachO, symbol.name().map_err(|_| ReadError::Object)?);
            if symbol.kind() == object::SymbolKind::Text && symbol.is_global() && name != function_name(module, alloc)? {
                return Ok(Destination::Named(target_of(module, name, alloc)?));
            }
            if let SymbolSection::Section(index) = symbol.section() {
                let section = file.section_by_index(index).map_err(|_| ReadError::NoSection)?;
                let offset = symbol.address().checked_sub(section.address()).and_then(|n| i64::try_from(n).ok()).ok_or(ReadError::CodeOutside)?;
                Ok(Destination::Section { index, offset })
            } else {
                Ok(Destination::Named(target_of(module, name, alloc)?))
            }
        }
        _ => Err(ReadError::Section { at }),
    }
}

fn local(index: SectionIndex, offset: i64, placed: &[(SectionIndex, u64)], text: SectionIndex, start: u64, at: u64) -> Result<(u32, i64), ReadError> {
    let base = placed.iter().find(|p| p.0 == index).ok_or(ReadError::Section { at })?.1;
    let adjustment = if index == text { -i64::try_from(start).map_err(|_| ReadError::CodeOutside)? } else { i64::try_from(base).map_err(|_| ReadError::CodeOutside)? };
    Ok((if index == text { OWN_CODE } else { OWN_RELRO }, offset.checked_add(adjustment).ok_or(ReadError::CodeOutside)?))
}

fn unsupported(relocation: &object::Relocation, at: u64) -> ReadError {
    let kind = match relocation.flags() {
        RelocationFlags::MachO { r_type, .. } => u32::from(r_type.0),
        RelocationFlags::Elf { r_type } => r_type.0,
        _ => 0,
    };
    ReadError::Relocation { at, kind }
}

fn word(bytes: &[u8], at: u64) -> Result<u32, ReadError> {
    let at = usize::try_from(at).map_err(|_| ReadError::CodeOutside)?;
    let bytes = bytes.get(at..at + 4).ok_or(ReadError::CodeOutside)?;
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| ReadError::CodeOutside)?))
}

fn low_kind(word: u32, at: u64) -> Result<SiteKind, ReadError> {
    if word & 0xffc0_0000 == 0x9100_0000 { return Ok(SiteKind::AddLo12); }
    if word & 0x3b00_0000 != 0x3900_0000 {
        return Err(ReadError::Semantics { at, what: "PAGEOFF12 is neither ADD64 nor an unsigned-offset load/store" });
    }
    let scale = if word >> 30 == 0 && word & 0x0480_0000 == 0x0480_0000 { 4 } else { word >> 30 };
    Ok(match scale { 0 => SiteKind::LdstLo12_1, 1 => SiteKind::LdstLo12_2, 2 => SiteKind::LdstLo12_4, 3 => SiteKind::LdstLo12_8, _ => SiteKind::LdstLo12_16 })
}

fn scale(kind: SiteKind) -> u32 {
    match kind { SiteKind::LdstLo12_2 => 1, SiteKind::LdstLo12_4 => 2, SiteKind::LdstLo12_8 | SiteKind::GotLo12 => 3, SiteKind::LdstLo12_16 => 4, _ => 0 }
}

fn instruction(file: &object::File<'_>, relocation: &object::Relocation, at: u64, dest: Destination, placed: &[(SectionIndex, u64)], text: SectionIndex, start: u64, record: &mut CodeRecord<'_>) -> Result<(), ReadError> {
    let original = word(&record.code, at)?;
    let kind = match relocation.flags() {
        RelocationFlags::MachO { r_type: macho::ARM64_RELOC_BRANCH26, r_pcrel: true, r_length: 2 } |
        RelocationFlags::Elf { r_type: elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 } => SiteKind::Branch26,
        RelocationFlags::MachO { r_type: macho::ARM64_RELOC_PAGE21, r_pcrel: true, r_length: 2 } |
        RelocationFlags::Elf { r_type: elf::R_AARCH64_ADR_PREL_PG_HI21 } => SiteKind::Page21,
        RelocationFlags::MachO { r_type: macho::ARM64_RELOC_GOT_LOAD_PAGE21, r_pcrel: true, r_length: 2 } |
        RelocationFlags::Elf { r_type: elf::R_AARCH64_ADR_GOT_PAGE } => SiteKind::GotPage21,
        RelocationFlags::MachO { r_type: macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12, r_pcrel: false, r_length: 2 } |
        RelocationFlags::Elf { r_type: elf::R_AARCH64_LD64_GOT_LO12_NC } => SiteKind::GotLo12,
        RelocationFlags::MachO { r_type: macho::ARM64_RELOC_PAGEOFF12, r_pcrel: false, r_length: 2 } => low_kind(original, at)?,
        RelocationFlags::Elf { r_type: elf::R_AARCH64_ADD_ABS_LO12_NC } => SiteKind::AddLo12,
        RelocationFlags::Elf { r_type: elf::R_AARCH64_LDST8_ABS_LO12_NC } => SiteKind::LdstLo12_1,
        RelocationFlags::Elf { r_type: elf::R_AARCH64_LDST16_ABS_LO12_NC } => SiteKind::LdstLo12_2,
        RelocationFlags::Elf { r_type: elf::R_AARCH64_LDST32_ABS_LO12_NC } => SiteKind::LdstLo12_4,
        RelocationFlags::Elf { r_type: elf::R_AARCH64_LDST64_ABS_LO12_NC } => SiteKind::LdstLo12_8,
        RelocationFlags::Elf { r_type: elf::R_AARCH64_LDST128_ABS_LO12_NC } => SiteKind::LdstLo12_16,
        _ => return Err(unsupported(relocation, at)),
    };
    let (mask, implicit) = match kind {
        SiteKind::Branch26 if original & 0x7c00_0000 == 0x1400_0000 => (0x03ff_ffff, (i64::from(original & 0x03ff_ffff) << 38 >> 38) * 4),
        SiteKind::Page21 | SiteKind::GotPage21 if original & 0x9f00_0000 == 0x9000_0000 => {
            let imm = ((original >> 29) & 3) | ((original >> 5) & 0x7ffff) << 2;
            (0x60ff_ffe0, (i64::from(imm) << 43 >> 43) * 4096)
        }
        SiteKind::GotLo12 if original & 0xffc0_0000 == 0xf940_0000 => (0x003f_fc00, i64::from((original >> 10) & 4095) * 8),
        kind if kind.own() && low_kind(original, at)? == kind => (0x003f_fc00, i64::from((original >> 10) & 4095) << scale(kind)),
        _ => return Err(ReadError::Semantics { at, what: "relocation kind does not match its AArch64 instruction" }),
    };
    let mut addend = relocation.addend();
    if relocation.has_implicit_addend() {
        addend = addend.checked_add(implicit).ok_or(ReadError::CodeOutside)?;
        // A section relocation's field is an address in the input object, not an offset
        // relative to a named symbol. A PAGE21 alone loses the low address bits; refuse
        // instead of guessing which later instruction consumes the page register.
        if let RelocationTarget::Section(index) = relocation.target() {
            if kind == SiteKind::Page21 {
                return Err(ReadError::Semantics { at, what: "section-relative PAGE21 has no symbol identifying its full address" });
            }
            let address = file.section_by_index(index).map_err(|_| ReadError::NoSection)?.address();
            addend = addend.checked_sub(i64::try_from(address).map_err(|_| ReadError::CodeOutside)?).ok_or(ReadError::CodeOutside)?;
        }
    }
    let (target, addend) = match dest {
        Destination::Section { index, offset } => {
            if matches!(kind, SiteKind::GotPage21 | SiteKind::GotLo12) {
                return Err(ReadError::Semantics { at, what: "GOT relocation to a locally defined symbol" });
            }
            local(index, offset.checked_add(addend).ok_or(ReadError::CodeOutside)?, placed, text, start, at)?
        }
        Destination::Named(target) if kind == SiteKind::Branch26 => {
            if addend != 0 { return Err(ReadError::Semantics { at, what: "external branch with a nonzero addend" }); }
            let veneer = u32::try_from(record.code.len()).map_err(|_| ReadError::CodeOutside)?;
            let target = record.target(target);
            // BL sets LR at its original site. The veneer tail-branches, preserving that
            // return address; a tail B also preserves its caller's LR.
            for word in [0x9000_0010u32, 0xf940_0210, 0xd61f_0200] { record.code.extend_from_slice(&word.to_le_bytes()); }
            record.sites.push(Site { at: veneer, kind: SiteKind::Load, target, addend: 0 });
            (OWN_CODE, i64::from(veneer))
        }
        Destination::Named(target) if matches!(kind, SiteKind::GotPage21 | SiteKind::GotLo12) && addend == 0 => (record.target(target), 0),
        Destination::Named(_) => return Err(ReadError::Semantics { at, what: "direct external address or nonzero GOT addend requires an address relocation" }),
    };
    let addend = u32::try_from(addend).map_err(|_| ReadError::Semantics { at, what: "local relocation is outside its record part" })?;
    let offset = u32::try_from(at).map_err(|_| ReadError::CodeOutside)?;
    let slot = usize::try_from(at).map_err(|_| ReadError::CodeOutside)?;
    record.code.get_mut(slot..slot + 4).ok_or(ReadError::CodeOutside)?.copy_from_slice(&(original & !mask).to_le_bytes());
    record.sites.push(Site { at: offset, kind, target, addend });
    Ok(())
}

fn data_relocation(file: &object::File<'_>, relocation: &object::Relocation, at: u64, dest: Destination, placed: &[(SectionIndex, u64)], text: SectionIndex, start: u64, record: &mut CodeRecord<'_>) -> Result<(), ReadError> {
    if !matches!(relocation.flags(), RelocationFlags::MachO { r_type: macho::ARM64_RELOC_UNSIGNED, r_pcrel: false, r_length: 3 } | RelocationFlags::Elf { r_type: elf::R_AARCH64_ABS64 }) {
        return Err(unsupported(relocation, at));
    }
    let slot = usize::try_from(at).map_err(|_| ReadError::CodeOutside)?;
    let bytes = record.relro.get(slot..slot + 8).ok_or(ReadError::CodeOutside)?;
    let mut addend = relocation.addend();
    if relocation.has_implicit_addend() {
        addend = addend.checked_add(i64::from_le_bytes(bytes.try_into().map_err(|_| ReadError::CodeOutside)?)).ok_or(ReadError::CodeOutside)?;
        if let RelocationTarget::Section(index) = relocation.target() {
            let address = file.section_by_index(index).map_err(|_| ReadError::NoSection)?.address();
            addend = addend.checked_sub(i64::try_from(address).map_err(|_| ReadError::CodeOutside)?).ok_or(ReadError::CodeOutside)?;
        }
    }
    let (target, addend) = match dest {
        Destination::Named(target) => (record.target(target), addend),
        Destination::Section { index, offset } => local(index, offset.checked_add(addend).ok_or(ReadError::CodeOutside)?, placed, text, start, at)?,
    };
    record.relro.get_mut(slot..slot + 8).ok_or(ReadError::CodeOutside)?.fill(0);
    record.words.push(Word { at: u32::try_from(at).map_err(|_| ReadError::CodeOutside)?, target, addend });
    Ok(())
}

fn position_name<'a>(module: &LlvmModule<'_>, row: u32, alloc: Alloc<'a>) -> Result<String<Alloc<'a>>, ReadError> {
    let mut name = String::from_str_in("blaze_pos_", alloc);
    name.push_str(&function_name(module, alloc)?);
    blaze_own::format_into!(&mut name, "_{row}");
    Ok(name)
}

fn position_section(file: &object::File<'_>, index: SectionIndex, module: &LlvmModule<'_>, alloc: Alloc<'_>) -> Result<bool, ReadError> {
    // LLVM marks only source-position globals writable. Do not admit arbitrary writable
    // globals just because they share a section with a position slot.
    let mut found = false;
    for symbol in file.symbols().filter(|symbol| symbol.section() == SymbolSection::Section(index) && symbol.kind() == object::SymbolKind::Data) {
        let name = plain(file.format() == BinaryFormat::MachO, symbol.name().map_err(|_| ReadError::Object)?);
        let mut matches = false;
        for (row, _) in &module.positions {
            matches |= name == position_name(module, *row, alloc)?;
        }
        if !matches { return Ok(false); }
        found = true;
    }
    Ok(found)
}

fn positions(file: &object::File<'_>, module: &LlvmModule<'_>, placed: &[(SectionIndex, u64)], record: &mut CodeRecord<'_>, alloc: Alloc<'_>) -> Result<(), ReadError> {
    for &(row, patch) in &module.positions {
        let name = position_name(module, row, alloc)?;
        let symbol = file.symbols().find(|symbol| symbol.name().is_ok_and(|s| plain(file.format() == BinaryFormat::MachO, s) == name));
        // An unused constant can be deleted by LLVM together with all its reads.
        let Some(symbol) = symbol else { continue };
        let SymbolSection::Section(index) = symbol.section() else { return Err(ReadError::NoSection) };
        let Some(&(_, base)) = placed.iter().find(|p| p.0 == index) else { continue };
        let section = file.section_by_index(index).map_err(|_| ReadError::NoSection)?;
        let at = symbol.address().checked_sub(section.address()).and_then(|n| n.checked_add(base)).and_then(|n| n.checked_add(u64::from(patch.offset))).and_then(|n| u32::try_from(n).ok()).ok_or(ReadError::CodeOutside)?;
        let field = match blaze_lir::func::PositionField::from_word(patch.field) {
            Some(blaze_lir::func::PositionField::Line) => PositionField::Line,
            Some(blaze_lir::func::PositionField::Col) => PositionField::Col,
            None => return Err(ReadError::Semantics { at: u64::from(at), what: "unknown source-position field" }),
        };
        record.positions.push(PositionRow { part: PositionPart::Relro, at, item: patch.item_key(), site: patch.site, field });
    }
    Ok(())
}

/// A symbol's name as the lowering lane spells it: Mach-O keeps a leading `_` on every
/// symbol, which LLVM adds and `object` does not take off.
fn plain<'n>(macho: bool, name: &'n str) -> &'n str {
    if macho { name.strip_prefix('_').unwrap_or(name) } else { name }
}

/// The `Target` of the symbol `name`, as the lowering lane's `references` names it.
fn target_of(module: &LlvmModule<'_>, name: &str, alloc: Alloc<'_>) -> Result<Target, ReadError> {
    let Some(target) = module.references.iter().flatten().chain(module.implicit_references.iter()).find(|t| LinkName(**t).spells(name, alloc)) else {
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
            Space::Outside => return self.0.symbol == blaze_code::record::Symbol::of_name(name),
        };
        let mut spelled = String::new_in(alloc);
        spelled.push_str(prefix);
        for b in self.0.symbol.0 {
            blaze_own::format_into!(&mut spelled, "{b:02x}");
        }
        &*spelled == name
    }
}
