//! The pinned AArch64 LLVM object's `eh_frame` and Rust personality tables. The object
//! relocations are resolved in section coordinates before adopting any CFI instruction.

use core::ops::Range;

use blaze_code::record::{CodeRecord, RecordError};
use blaze_code::trap;
use blaze_code::unwind::{self, Action, Row};
use blaze_code::unwind::cfi::{self, CfiInput};
use blaze_own::{Alloc, Vec};
use object::{
    BinaryFormat, Object, ObjectSection, ObjectSymbol, Relocation, RelocationFlags,
    RelocationTarget, SectionIndex, SymbolSection, elf, macho,
};

use super::{control::CLEANUP_LANDING, object::ReadError};

const OMIT: u8 = 0xff;
const PCREL_PTR: u8 = 0x10;
const PCREL_SDATA4: u8 = 0x1b;
const PCREL_SDATA8: u8 = 0x1c;
const INDIRECT: u8 = 0x80;

type Relocs<'a> = &'a [(u64, Relocation)];

fn bad(at: usize, what: &'static str) -> ReadError {
    ReadError::Unwind { at, what }
}

struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        let end =
            self.at.checked_add(n).ok_or_else(|| bad(self.at, "unwind field length overflows"))?;
        let bytes =
            self.bytes.get(self.at..end).ok_or_else(|| bad(self.at, "truncated unwind field"))?;
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, ReadError> {
        Ok(self.take(1)?[0])
    }

    fn fixed(&mut self, n: usize) -> Result<u64, ReadError> {
        let mut bytes = [0; 8];
        bytes[..n].copy_from_slice(self.take(n)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn leb(&mut self, signed: bool) -> Result<u64, ReadError> {
        let at = self.at;
        let mut value = 0;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            let bits = byte & 0x7f;
            let overflows = if signed { bits != 0 && bits != 0x7f } else { bits > 1 };
            if shift == 63 && (byte & 0x80 != 0 || overflows) {
                return Err(bad(at, "unwind LEB128 overflows"));
            }
            value |= u64::from(bits) << shift;
            if byte & 0x80 == 0 {
                if signed && byte & 0x40 != 0 && shift < 63 {
                    value |= u64::MAX << (shift + 7);
                }
                return Ok(value);
            }
        }
        Err(bad(at, "unterminated unwind LEB128"))
    }

    fn size(&mut self) -> Result<usize, ReadError> {
        usize::try_from(self.leb(false)?).map_err(|_| bad(self.at, "unwind length exceeds usize"))
    }

    fn end(&self, n: usize) -> Result<usize, ReadError> {
        self.at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| bad(self.at, "unwind range exceeds its entry"))
    }

    fn offsets(&mut self, encoding: u8) -> Result<u32, ReadError> {
        let at = self.at;
        let value = match encoding {
            0x01 => self.leb(false)?,
            0x02 => self.fixed(2)?,
            0x03 => self.fixed(4)?,
            0x00 | 0x04 => self.fixed(8)?,
            _ => return Err(bad(at, "unsupported LSDA call-site encoding")),
        };
        u32::try_from(value).map_err(|_| bad(at, "LSDA call-site offset exceeds the record form"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Location {
    section: SectionIndex,
    offset: u64,
}

fn location(
    file: &object::File<'_>,
    target: RelocationTarget,
    at: usize,
) -> Result<Location, ReadError> {
    match target {
        RelocationTarget::Section(section) => Ok(Location { section, offset: 0 }),
        RelocationTarget::Symbol(index) => {
            let symbol = file
                .symbol_by_index(index)
                .map_err(|_| bad(at, "unknown unwind relocation symbol"))?;
            let SymbolSection::Section(section) = symbol.section() else {
                return Err(bad(at, "unwind relocation needs a defined symbol"));
            };
            let base = file
                .section_by_index(section)
                .map_err(|_| bad(at, "unknown unwind relocation section"))?
                .address();
            let offset = symbol
                .address()
                .checked_sub(base)
                .ok_or_else(|| bad(at, "unwind symbol lies below its section"))?;
            Ok(Location { section, offset })
        }
        _ => Err(bad(at, "unsupported unwind relocation target")),
    }
}

fn relocation(rows: Relocs<'_>, at: usize) -> Option<&Relocation> {
    let index = rows.partition_point(|(offset, _)| *offset < at as u64);
    rows.get(index).filter(|(offset, _)| *offset == at as u64).map(|(_, reloc)| reloc)
}

fn only_relocations(
    rows: Relocs<'_>,
    range: Range<usize>,
    fields: &[usize],
) -> Result<(), ReadError> {
    let first = rows.partition_point(|(at, _)| *at < range.start as u64);
    let end = rows.partition_point(|(at, _)| *at < range.end as u64);
    let mut previous = None;
    for &(at, _) in &rows[first..end] {
        let at = at as usize;
        if previous == Some(at) || !fields.contains(&at) {
            return Err(bad(at, "unexpected relocation in unwind entry"));
        }
        previous = Some(at);
    }
    Ok(())
}

// `object` combines the pair for us. Check the original shape before relying on it,
// since a SUBTRACTOR can otherwise get attached to a relocation at another field.
fn check_macho_relocations(file: &object::File<'_>, frame: SectionIndex) -> Result<(), ReadError> {
    let object::File::MachO64(file) = file else { return Ok(()) };
    let section =
        file.section_by_index(frame).map_err(|_| bad(0, "unknown Mach-O frame section"))?;
    let mut rows = section
        .macho_relocations()
        .map_err(|_| bad(0, "unreadable Mach-O frame relocations"))?
        .iter();
    let endian = file.endian();
    while let Some(row) = rows.next() {
        let row = row.info(endian);
        let at = row.r_address as usize;
        match row.r_type {
            macho::ARM64_RELOC_SUBTRACTOR
                if row.r_extern && !row.r_pcrel && matches!(row.r_length, 2 | 3) =>
            {
                let plus = rows
                    .next()
                    .ok_or_else(|| bad(at, "missing Mach-O unwind UNSIGNED pair"))?
                    .info(endian);
                if plus.r_type != macho::ARM64_RELOC_UNSIGNED
                    || plus.r_address != row.r_address
                    || plus.r_length != row.r_length
                    || !plus.r_extern
                    || plus.r_pcrel
                {
                    return Err(bad(at, "invalid Mach-O unwind SUBTRACTOR/UNSIGNED pair"));
                }
            }
            macho::ARM64_RELOC_POINTER_TO_GOT
                if row.r_extern && row.r_pcrel && row.r_length == 2 => {}
            _ => return Err(bad(at, "unsupported Mach-O unwind relocation")),
        }
    }
    Ok(())
}

fn pointer_width(encoding: u8, at: usize) -> Result<usize, ReadError> {
    match encoding & !INDIRECT {
        PCREL_SDATA4 => Ok(4),
        PCREL_PTR | PCREL_SDATA8 => Ok(8),
        _ => Err(bad(at, "unsupported DWARF pointer encoding")),
    }
}

fn elf_pcrel(reloc: &Relocation, width: usize) -> bool {
    let kind = if width == 4 { elf::R_AARCH64_PREL32 } else { elf::R_AARCH64_PREL64 };
    matches!(reloc.flags(), RelocationFlags::Elf { r_type } if r_type == kind)
        && !reloc.has_implicit_addend()
        && reloc.subtractor().is_none()
}

fn pointer(
    file: &object::File<'_>,
    frame: SectionIndex,
    rows: Relocs<'_>,
    bytes: &mut Bytes<'_>,
    encoding: u8,
) -> Result<Option<Location>, ReadError> {
    let at = bytes.at;
    let width = pointer_width(encoding, at)?;
    let raw = bytes.fixed(width)?;
    let Some(reloc) = relocation(rows, at) else {
        return if raw == 0 {
            Ok(None)
        } else {
            Err(bad(at, "nonzero unwind pointer has no relocation"))
        };
    };
    if encoding & INDIRECT != 0 {
        return Err(bad(at, "indirect function or LSDA pointer"));
    }
    let to = location(file, reloc.target(), at)?;
    let addend = match file.format() {
        BinaryFormat::Elf if raw == 0 && elf_pcrel(reloc, width) => i128::from(reloc.addend()),
        BinaryFormat::MachO if matches!(reloc.flags(), RelocationFlags::MachO { r_type: macho::ARM64_RELOC_UNSIGNED, r_pcrel: false, r_length } if 1usize << r_length == width) =>
        {
            let minus = reloc
                .subtractor()
                .ok_or_else(|| bad(at, "Mach-O PC-relative pointer has no subtractor"))?;
            let from = location(file, RelocationTarget::Symbol(minus), at)?;
            if from.section != frame
                || from.offset > at as u64
                || reloc.addend() != 0
                || !reloc.has_implicit_addend()
            {
                return Err(bad(at, "invalid Mach-O unwind subtractor or addend"));
            }
            // The encoded difference is to the relocated field, whereas Mach-O names the
            // section's local anchor as the subtracted symbol.
            let signed = if width == 4 { i128::from(raw as i32) } else { i128::from(raw as i64) };
            signed + at as i128 - i128::from(from.offset)
        }
        _ => return Err(bad(at, "unsupported unwind pointer relocation")),
    };
    let offset = u64::try_from(i128::from(to.offset) + addend)
        .map_err(|_| bad(at, "unwind relocation lies below its target section"))?;
    Ok(Some(Location { offset, ..to }))
}

fn personality(
    file: &object::File<'_>,
    rows: Relocs<'_>,
    bytes: &mut Bytes<'_>,
    encoding: u8,
) -> Result<(), ReadError> {
    let at = bytes.at;
    let width = pointer_width(encoding, at)?;
    let raw = bytes.fixed(width)?;
    let reloc =
        relocation(rows, at).ok_or_else(|| bad(at, "Rust personality has no relocation"))?;
    let target = match file.format() {
        BinaryFormat::Elf
            if encoding == INDIRECT | PCREL_SDATA8
                && raw == 0
                && elf_pcrel(reloc, width)
                && reloc.addend() == 0 =>
        {
            let cell = location(file, reloc.target(), at)?;
            let section = file
                .section_by_index(cell.section)
                .map_err(|_| bad(at, "Rust personality cell has no section"))?;
            let mut relocs = section.relocations().filter(|(offset, _)| *offset == cell.offset);
            let (_, reloc) =
                relocs.next().ok_or_else(|| bad(at, "Rust personality cell has no relocation"))?;
            if relocs.next().is_some()
                || reloc.flags() != (RelocationFlags::Elf { r_type: elf::R_AARCH64_ABS64 })
                || reloc.has_implicit_addend()
                || reloc.addend() != 0
            {
                return Err(bad(at, "unsupported Rust personality cell relocation"));
            }
            reloc.target()
        }
        BinaryFormat::MachO
            if encoding == INDIRECT | PCREL_SDATA4
                && reloc.addend() == 0
                && reloc.subtractor().is_none()
                && matches!(
                    reloc.flags(),
                    RelocationFlags::MachO {
                        r_type: macho::ARM64_RELOC_POINTER_TO_GOT,
                        r_pcrel: true,
                        r_length: 2
                    }
                ) =>
        {
            reloc.target()
        }
        _ => return Err(bad(at, "unsupported Rust personality encoding or relocation")),
    };
    let RelocationTarget::Symbol(index) = target else {
        return Err(bad(at, "Rust personality is not a named symbol"));
    };
    let symbol =
        file.symbol_by_index(index).map_err(|_| bad(at, "unknown Rust personality symbol"))?;
    let expected = if file.format() == BinaryFormat::MachO {
        "_rust_eh_personality"
    } else {
        "rust_eh_personality"
    };
    if symbol.name().ok() != Some(expected) || !symbol.is_undefined() {
        return Err(bad(at, "unwind personality differs from the program's Rust personality"));
    }
    Ok(())
}

struct Cie<'a> {
    code_align: u8,
    data_align: i8,
    pc: u8,
    lsda: Option<u8>,
    initial: &'a [u8],
}

fn cie<'a>(
    file: &object::File<'_>,
    rows: Relocs<'_>,
    frame: &'a [u8],
    at: usize,
) -> Result<Cie<'a>, ReadError> {
    let mut bytes = Bytes { bytes: frame, at };
    let len = bytes.fixed(4)? as usize;
    let end = bytes.end(len)?;
    bytes.bytes = &frame[..end];
    if bytes.fixed(4)? != 0 || bytes.byte()? != 1 {
        return Err(bad(at, "expected a DWARF32 version 1 CIE"));
    }
    let aug = match bytes.take(3)? {
        b"zR\0" => false,
        b"zPL" if bytes.take(2)? == b"R\0" => true,
        _ => return Err(bad(at, "unsupported CIE augmentation")),
    };
    let code_align = u8::try_from(bytes.leb(false)?)
        .map_err(|_| bad(at, "CIE code alignment exceeds the record form"))?;
    let data_align = i8::try_from(bytes.leb(true)? as i64)
        .map_err(|_| bad(at, "CIE data alignment exceeds the record form"))?;
    if bytes.byte()? != 30 {
        return Err(bad(at, "CIE return address register is not x30"));
    }
    let len = bytes.size()?;
    let aug_end = bytes.end(len)?;
    let (lsda, allowed) = if aug {
        let encoding = bytes.byte()?;
        let field = bytes.at;
        personality(file, rows, &mut bytes, encoding)?;
        (Some(bytes.byte()?), Some(field))
    } else {
        (None, None)
    };
    let pc = bytes.byte()?;
    if bytes.at != aug_end {
        return Err(bad(at, "unexpected bytes in CIE augmentation"));
    }
    only_relocations(rows, at..end, allowed.as_slice())?;
    Ok(Cie { code_align, data_align, pc, lsda, initial: &frame[bytes.at..end] })
}

pub(super) struct Unwind<'a> {
    input: CfiInput<'a>,
    lsda: Option<Location>,
}

impl Unwind<'_> {
    pub(super) fn extent(&self) -> u64 {
        u64::from(self.input.code_len)
    }

    pub(super) fn write(
        &self,
        file: &object::File<'_>,
        record: &mut CodeRecord<'_>,
        alloc: Alloc<'_>,
    ) -> Result<(), ReadError> {
        let code = u32::try_from(record.code.len()).map_err(|_| ReadError::CodeOutside)?;
        cfi::write(self.input, code, &mut record.machine_cfi).map_err(ReadError::UnwindCfi)?;
        if let Some(location) = self.lsda {
            lsda(file, location, record)?;
            omit_cleanup_landings(record, code, alloc)?;
        }
        Ok(())
    }
}

/// A compiler-created landing gives the exact machine range that must have no LSDA row.
/// This preserves the stencil's fatal unwind action even during the search or a forced
/// unwind. The CFI and all other LSDA ranges remain LLVM's.
fn omit_cleanup_landings(
    record: &mut CodeRecord<'_>,
    code_len: u32,
    alloc: Alloc<'_>,
) -> Result<(), ReadError> {
    let mut marked = Vec::new_in(alloc);
    for pad in &record.pads {
        let at = pad.landing as usize;
        if at == 0 {
            continue;
        }
        let word = record.code.get(at..).and_then(|bytes| bytes.first_chunk::<4>());
        if word.and_then(|word| trap::brk(u32::from_le_bytes(*word))) == Some(CLEANUP_LANDING) {
            if pad.action != Action::Terminate.code() {
                return Err(bad(at, "the cleanup marker's LSDA action is not terminate"));
            }
            marked.push(pad.landing);
        }
    }
    if marked.is_empty() {
        return Ok(());
    }
    // Validate the entire call-site table before dropping any row, including overlap with
    // the ranges that remain. The parser preserves the order of the machine's entries.
    let cfi = cfi::Cfi::read(&record.machine_cfi, code_len).map_err(ReadError::UnwindCfi)?;
    unwind::check(None, Some(cfi), record.pads.len(), u64::from(code_len), |i| record.pads[i])
        .map_err(|error| ReadError::Seal(RecordError::Unwind(error)))?;
    marked.sort_unstable();
    marked.dedup();
    record.pads.retain(|pad| marked.binary_search(&pad.landing).is_err());
    let abort = (0xd420_0000 | (trap::ABORT << 5)).to_le_bytes();
    for landing in marked {
        let at = landing as usize;
        // The marker lookup above checked these four bytes, and the table check aligned them.
        record.code[at..at + 4].copy_from_slice(&abort);
    }
    Ok(())
}

/// Selects the only FDE for the root symbol. ELF also gives a function size in its symbol
/// table. Mach-O gives the bound of the atom, followed by alignment or the next symbol.
pub(super) fn find<'a>(
    file: &object::File<'a>,
    text: SectionIndex,
    atom: Range<u64>,
    alloc: Alloc<'_>,
) -> Result<Unwind<'a>, ReadError> {
    let name = if file.format() == BinaryFormat::MachO { "__eh_frame" } else { ".eh_frame" };
    let section =
        file.section_by_name(name).ok_or_else(|| bad(0, "function has no DWARF frame section"))?;
    check_macho_relocations(file, section.index())?;
    let frame = section.data().map_err(|_| bad(0, "unreadable DWARF frame section"))?;
    let mut relocs = Vec::new_in(alloc);
    relocs.extend(section.relocations());
    relocs.sort_unstable_by_key(|(at, _)| *at);
    if relocs.last().is_some_and(|(at, _)| *at >= frame.len() as u64) {
        return Err(bad(frame.len(), "unwind relocation lies outside the frame section"));
    }
    let mut cies = Vec::new_in(alloc);
    let mut at = 0;
    let mut found = None;
    while at < frame.len() {
        let mut bytes = Bytes { bytes: frame, at };
        let len = bytes.fixed(4)? as usize;
        if len == 0 && frame[at..].iter().all(|&byte| byte == 0) {
            only_relocations(&relocs, at..frame.len(), &[])?;
            break;
        }
        if len < 4 || len == u32::MAX as usize {
            return Err(bad(at, "unsupported DWARF frame entry length"));
        }
        let end = bytes.end(len)?;
        let cie_field = bytes.at;
        let back = bytes.fixed(4)? as usize;
        at = end;
        if back == 0 {
            let start = cie_field - 4;
            cies.push((start, cie(file, &relocs, frame, start)?));
            continue;
        }
        let cie_at = cie_field
            .checked_sub(back)
            .ok_or_else(|| bad(cie_field, "invalid backward CIE pointer"))?;
        let index = cies
            .binary_search_by_key(&cie_at, |(at, _)| *at)
            .map_err(|_| bad(cie_field, "FDE pointer does not name a CIE"))?;
        let cie = &cies[index].1;
        bytes.bytes = &frame[..end];
        let pc_field = bytes.at;
        let start = pointer(file, section.index(), &relocs, &mut bytes, cie.pc)?
            .ok_or_else(|| bad(pc_field, "FDE has no function relocation"))?;
        let width = pointer_width(cie.pc, bytes.at)?;
        let extent = bytes.fixed(width)?;
        if start.section != text {
            continue;
        }
        let past = start
            .offset
            .checked_add(extent)
            .ok_or_else(|| bad(pc_field, "FDE extent overflows"))?;
        if start.offset != atom.start {
            if past > atom.start && start.offset < atom.end {
                return Err(bad(pc_field, "another FDE intersects the selected function atom"));
            }
            continue;
        }
        if found.is_some()
            || extent == 0
            || past > atom.end
            || file.format() == BinaryFormat::Elf && past != atom.end
        {
            return Err(bad(
                pc_field,
                "FDE is duplicated or disagrees with the function symbol extent",
            ));
        }
        let code_len = u32::try_from(extent)
            .map_err(|_| bad(pc_field, "FDE extent exceeds the code record"))?;
        let aug_len = bytes.size()?;
        let aug_end = bytes.end(aug_len)?;
        let lsda_field = bytes.at;
        let lsda = match cie.lsda {
            Some(encoding) => pointer(file, section.index(), &relocs, &mut bytes, encoding)?,
            None => None,
        };
        if bytes.at != aug_end {
            return Err(bad(pc_field, "unexpected bytes in FDE augmentation"));
        }
        let fields = [pc_field, lsda_field];
        only_relocations(
            &relocs,
            cie_field - 4..end,
            &fields[..if cie.lsda.is_some() { 2 } else { 1 }],
        )?;
        let input = CfiInput {
            code_len,
            code_align: cie.code_align,
            data_align: cie.data_align,
            has_lsda: lsda.is_some(),
            initial: cie.initial,
            instructions: &frame[bytes.at..end],
        };
        found = Some(Unwind { input, lsda });
    }
    found.ok_or_else(|| bad(0, "function has no matching authoritative FDE"))
}

fn lsda(
    file: &object::File<'_>,
    location: Location,
    record: &mut CodeRecord<'_>,
) -> Result<(), ReadError> {
    let section =
        file.section_by_index(location.section).map_err(|_| bad(0, "unknown LSDA section"))?;
    let name =
        if file.format() == BinaryFormat::MachO { "__gcc_except_tab" } else { ".gcc_except_table" };
    if section.name().ok() != Some(name)
        && !section.name().is_ok_and(|name| name.starts_with(".gcc_except_table."))
    {
        return Err(bad(0, "LSDA relocation does not name an exception-table section"));
    }
    let data = section.data().map_err(|_| bad(0, "unreadable LSDA section"))?;
    let at = usize::try_from(location.offset).map_err(|_| bad(0, "LSDA location exceeds usize"))?;
    let end = file
        .symbols()
        .filter(|symbol| symbol.section_index() == Some(location.section))
        .filter(|symbol| symbol.name().is_ok_and(|name| name.starts_with("GCC_except_table")))
        .filter_map(|symbol| symbol.address().checked_sub(section.address()))
        .filter_map(|offset| usize::try_from(offset).ok())
        .filter(|&offset| offset > at)
        .min()
        .unwrap_or(data.len());
    let data =
        data.get(..end).ok_or_else(|| bad(at, "LSDA symbol bound lies outside its section"))?;
    let mut bytes = Bytes { bytes: data, at };
    if bytes.byte()? != OMIT {
        return Err(bad(at, "LSDA landing-pad base differs from the function start"));
    }
    let types = bytes.byte()?;
    let (type_width, type_base) = if types == OMIT {
        (0, data.len())
    } else {
        let width = pointer_width(types, bytes.at - 1)?;
        let len = bytes.size()?;
        (width, bytes.end(len)?)
    };
    let encoding = bytes.byte()?;
    let len = bytes.size()?;
    let actions = bytes.end(len)?;
    if actions > type_base {
        return Err(bad(at, "LSDA call-site table overlaps its type table"));
    }
    bytes.bytes = &data[..actions];
    let mut first_type = type_base;
    let mut last_action = actions;
    while bytes.at < actions {
        let start = bytes.offsets(encoding)?;
        let len = bytes.offsets(encoding)?;
        let landing = bytes.offsets(encoding)?;
        let action_entry = bytes.size()?;
        let action = match (landing, action_entry) {
            (0, _) => Action::Continue,
            (_, 0) => Action::Cleanup,
            _ => {
                let at = actions
                    .checked_add(action_entry - 1)
                    .ok_or_else(|| bad(actions, "LSDA action index overflows"))?;
                let mut action = Bytes { bytes: &data[..type_base], at };
                let index = action.leb(true)? as i64;
                let next_at = action.at;
                let next = action.leb(true)? as i64;
                last_action = last_action.max(action.at);
                if index > 0 {
                    let n = usize::try_from(index)
                        .ok()
                        .and_then(|n| n.checked_mul(type_width))
                        .filter(|&n| n != 0)
                        .ok_or_else(|| bad(at, "LSDA action names an invalid type index"))?;
                    first_type = first_type.min(
                        type_base
                            .checked_sub(n)
                            .ok_or_else(|| bad(at, "LSDA type index exceeds its table"))?,
                    );
                }
                match (index, next) {
                    (_, next) if next != 0 => {
                        let to = usize::try_from(next_at as i128 + i128::from(next))
                            .ok()
                            .filter(|&p| p >= actions && p < type_base)
                            .ok_or_else(|| {
                                bad(next_at, "LSDA action chain leaves its action table")
                            })?;
                        last_action = last_action.max(to + 1);
                        // The pinned Rust personality treats a multi-action landing as Catch.
                        Action::Catch
                    }
                    (0, _) => Action::Cleanup,
                    (n, _) if n > 0 => Action::Catch,
                    _ => Action::Terminate,
                }
            }
        };
        record.pads.push(Row { start, len, landing, action }.pad());
    }
    if last_action > first_type {
        return Err(bad(last_action, "LSDA actions overlap their type table"));
    }
    if section.relocations().any(|(offset, _)| (at as u64..last_action as u64).contains(&offset)) {
        return Err(bad(at, "LSDA call-site or action data contains a relocation"));
    }
    Ok(())
}
