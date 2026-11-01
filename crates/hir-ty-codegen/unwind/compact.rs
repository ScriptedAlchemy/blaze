//! Mach-O's linker input for unwinding. Keep the producer's compact encoding, or point
//! explicitly at our FDE: synthesizing compact frames from CFI is linker-dependent.

use cranelift_codegen::gimli::{self, UnwindSection};
use cranelift_module::{DataId, FuncId};
use cranelift_object::{
    ObjectProduct,
    object::{
        Architecture, BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind,
        SectionFlags, SectionKind, macho, write::Relocation,
    },
};

const MODE: u32 = 0x0f00_0000;
const HAS_LSDA: u32 = 0x4000_0000;
const PERSONALITY: u32 = 0x3000_0000;
const FDE_OFFSET: u32 = 0x00ff_ffff;

#[derive(Default)]
pub(super) struct Table {
    pub(super) personality: Option<FuncId>,
    /// In the same order as the FDEs in the frame table.
    entries: Vec<Entry>,
}

struct Entry {
    func: FuncId,
    encoding: Option<u32>,
    lsda: Option<DataId>,
}

impl Table {
    pub(super) fn add_function(
        &mut self,
        func: FuncId,
        encoding: Option<u32>,
        lsda: Option<DataId>,
    ) {
        if self.personality.is_some() {
            self.entries.push(Entry { func, encoding, lsda });
        }
    }

    pub(super) fn emit(self, product: &mut ObjectProduct, eh_frame: &[u8]) -> Result<(), String> {
        if product.object.format() != BinaryFormat::MachO || self.entries.is_empty() {
            return Ok(());
        }
        let dwarf = match product.object.architecture() {
            Architecture::Aarch64 => 0x0300_0000,
            Architecture::X86_64 => 0x0400_0000,
            _ => return Err("compact unwind for an unsupported Mach-O architecture".to_owned()),
        };
        let section = product.object.add_section(
            b"__LD".to_vec(),
            b"__compact_unwind".to_vec(),
            SectionKind::ReadOnlyData,
        );
        // Linker metadata, consumed into __unwind_info rather than mapped into the image.
        product.object.section_mut(section).flags =
            SectionFlags::MachO { flags: macho::S_ATTR_DEBUG, reserved2: 0 };
        let frames = gimli::EhFrame::new(eh_frame, gimli::LittleEndian);
        let bases = gimli::BaseAddresses::default().set_eh_frame(0);
        let mut fdes = frames.entries(&bases);
        let mut entries = self.entries.into_iter();
        let mut data = Vec::new();
        let mut relocs = Vec::new();
        while let Some(frame) = fdes.next().map_err(|e| e.to_string())? {
            let gimli::CieOrFde::Fde(frame) = frame else { continue };
            let entry = entries.next().ok_or("FDE without a compact unwind entry")?;
            let fde = frame
                .parse(|_, bases, offset| frames.cie_from_offset(bases, offset))
                .map_err(|e| e.to_string())?;
            let encoding = encoding(entry.encoding, dwarf, fde.offset(), entry.lsda.is_some())?;
            let at = data.len() as u64;
            data.extend_from_slice(&[0; 8]);
            data.extend_from_slice(&(fde.len() as u32).to_le_bytes());
            data.extend_from_slice(&encoding.to_le_bytes());
            data.extend_from_slice(&[0; 16]);
            relocs.push((at, product.function_symbol(entry.func)));
            if let Some(lsda) = entry.lsda {
                let personality = self.personality.ok_or("compact unwind without a personality")?;
                relocs.push((at + 16, product.function_symbol(personality)));
                relocs.push((at + 24, product.data_symbol(lsda)));
            }
        }
        if entries.next().is_some() {
            return Err("compact unwind entry without an FDE".to_owned());
        }
        product.object.section_mut(section).set_data(data, 8);
        for (offset, symbol) in relocs {
            product
                .object
                .add_relocation(
                    section,
                    Relocation {
                        offset,
                        symbol,
                        flags: RelocationFlags::Generic {
                            kind: RelocationKind::Absolute,
                            encoding: RelocationEncoding::Generic,
                            size: 64,
                        },
                        addend: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn encoding(compact: Option<u32>, dwarf: u32, fde: usize, lsda: bool) -> Result<u32, String> {
    let frame = match compact {
        // The old FDE offset belongs to the producer's object, not the stitched one.
        Some(encoding) if encoding & MODE != dwarf => encoding,
        _ => {
            let offset = u32::try_from(fde)
                .ok()
                .filter(|offset| *offset <= FDE_OFFSET)
                .ok_or("Mach-O compact unwind FDE offset exceeds 24 bits")?;
            dwarf | offset
        }
    };
    // Personalities are pointers in object files; the linker assigns their indices in
    // the final image. The LSDA flag follows the same association as our DWARF entry.
    Ok((frame & !(PERSONALITY | HAS_LSDA)) | if lsda { HAS_LSDA } else { 0 })
}

#[cfg(test)]
mod tests;
