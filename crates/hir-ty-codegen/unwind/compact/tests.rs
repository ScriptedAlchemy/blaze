use super::*;
use crate::unwind::{FrameRows, FrameRule, FunctionUnwind, UnwindContext};
use cranelift_codegen::{Context, ir::InstBuilder, settings};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use object::{Object, ObjectSection, ObjectSymbol};

fn module(triple: &str) -> ObjectModule {
    let isa = cranelift_codegen::isa::lookup(triple.parse().unwrap())
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap();
    ObjectModule::new(
        ObjectBuilder::new(isa, "compact-unwind", cranelift_module::default_libcall_names())
            .unwrap(),
    )
}

fn function(module: &mut ObjectModule, name: &str) -> (FuncId, FunctionUnwind) {
    let mut ctx = Context::new();
    ctx.func.signature = module.make_signature();
    let id = module.declare_function(name, Linkage::Local, &ctx.func.signature).unwrap();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
    let entry = builder.create_block();
    builder.switch_to_block(entry);
    builder.ins().return_(&[]);
    builder.seal_all_blocks();
    builder.finalize(module.target_config());
    module.define_function(id, &mut ctx).unwrap();
    let unwind = FunctionUnwind::of(module.isa(), &ctx).unwrap().unwrap();
    (id, unwind)
}

#[test]
fn macho_entries_match_fdes_and_landing_pads() {
    for (triple, dwarf, native) in [
        ("aarch64-apple-darwin", 0x0300_0000, 0x0400_0103),
        ("x86_64-apple-darwin", 0x0400_0000, 0x0102_0001),
    ] {
        let mut module = module(triple);
        let mut tables = UnwindContext::new(&mut module).unwrap();
        // Mix CIEs, compact and DWARF modes, and leave gaps between function ids.
        module.declare_function("unused", Linkage::Import, &module.make_signature()).unwrap();
        for (index, (compact, pads)) in [
            (None, false),
            (Some(native), true),
            (None, true),
            (Some(dwarf | 0x1234), false),
            (Some(native), false),
        ]
        .into_iter()
        .enumerate()
        {
            let (id, mut unwind) = function(&mut module, &format!("frame{index}"));
            if let Some(encoding) = compact {
                // Use the same constructor as LLVM's compact-entry reader. This does not
                // need libLLVM, so Cranelift-only builds cover the encoding carry-through.
                unwind = FunctionUnwind::from_rows(
                    FrameRows { len: 4, rows: vec![(0, FrameRule::CfaOffset(0))] },
                    None,
                )
                .with_compact_encoding(encoding);
            }
            unwind.landing_pads = pads.then(|| vec![0xff, 0xff, 1, 0]);
            // The encoding must also survive the function cache's serialization.
            let bytes = postcard::to_allocvec(&unwind).unwrap();
            let unwind: FunctionUnwind = postcard::from_bytes(&bytes).unwrap();
            tables.add_function(&mut module, id, &unwind).unwrap();
        }
        let mut product = module.finish();
        tables.emit(&mut product).unwrap();
        let bytes = product.emit().unwrap();
        let file = object::File::parse(bytes.as_slice()).unwrap();
        let compact = file.section_by_name("__compact_unwind").unwrap();
        assert_eq!(compact.segment_name().unwrap(), Some("__LD"));
        assert_eq!(compact.align(), 8);
        assert_eq!(
            compact.flags(),
            object::SectionFlags::MachO { flags: object::macho::S_ATTR_DEBUG }
        );
        let compact_data = compact.data().unwrap();
        assert_eq!(compact_data.len(), 5 * 32);
        let eh = file.section_by_name("__eh_frame").unwrap();
        let frames = gimli::EhFrame::new(eh.data().unwrap(), gimli::LittleEndian);
        let bases = gimli::BaseAddresses::default().set_eh_frame(0);
        let mut fdes = frames.entries(&bases);
        let mut index = 0;
        while let Some(frame) = fdes.next().unwrap() {
            let gimli::CieOrFde::Fde(frame) = frame else { continue };
            let fde =
                frame.parse(|_, bases, offset| frames.cie_from_offset(bases, offset)).unwrap();
            let entry = &compact_data[index * 32..][..32];
            let len = u32::from_le_bytes(entry[8..12].try_into().unwrap());
            let encoding = u32::from_le_bytes(entry[12..16].try_into().unwrap());
            assert_eq!(u64::from(len), fde.len());
            let has_pads = index == 1 || index == 2;
            assert_eq!(encoding & HAS_LSDA != 0, has_pads);
            assert_eq!(fde.lsda().is_some(), has_pads);
            assert_eq!(fde.cie().personality().is_some(), has_pads);
            if index == 1 || index == 4 {
                assert_eq!(encoding & !(HAS_LSDA | PERSONALITY), native);
            } else {
                assert_eq!(encoding & MODE, dwarf);
                assert_eq!((encoding & FDE_OFFSET) as usize, fde.offset());
            }
            let at = (index * 32) as u64;
            let relocs: Vec<_> =
                compact.relocations().filter(|(offset, _)| *offset / 32 == index as u64).collect();
            assert_eq!(relocs.len(), if has_pads { 3 } else { 1 });
            for (offset, reloc) in &relocs {
                assert_eq!(reloc.kind(), object::RelocationKind::Absolute);
                assert_eq!(reloc.size(), 64);
                assert!(reloc.has_implicit_addend());
                assert!([at, at + 16, at + 24].contains(offset));
            }
            let symbol_at = |section: &object::Section<'_, '_>, at| {
                // arm64 FDE pointers have subtractor/unsigned pairs; x86-64 uses
                // signed PC-relative relocations. Both name the compact entry's symbol.
                section
                    .relocations()
                    .find_map(|(offset, reloc)| {
                        if offset != at
                            || !matches!(
                                reloc.kind(),
                                object::RelocationKind::Absolute | object::RelocationKind::Relative
                            )
                        {
                            return None;
                        }
                        let object::RelocationTarget::Symbol(symbol) = reloc.target() else {
                            return None;
                        };
                        Some(file.symbol_by_index(symbol).unwrap().name().unwrap().to_owned())
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "{} relocation at {at}: {:?}",
                            section.name().unwrap(),
                            section.relocations().collect::<Vec<_>>()
                        )
                    })
            };
            assert_eq!(symbol_at(&compact, at), format!("_frame{index}"));
            assert_eq!(symbol_at(&eh, fde.offset() as u64 + 8), symbol_at(&compact, at));
            if has_pads {
                let cie = fde.cie();
                let personality = eh
                    .relocations()
                    .find_map(|(offset, reloc)| {
                        if !(cie.offset() as u64..cie.offset() as u64 + cie.entry_len() as u64 + 4)
                            .contains(&offset)
                        {
                            return None;
                        }
                        let object::RelocationTarget::Symbol(symbol) = reloc.target() else {
                            return None;
                        };
                        Some(file.symbol_by_index(symbol).unwrap().name().unwrap().to_owned())
                    })
                    .unwrap();
                assert_eq!(personality, "_rust_eh_personality");
                assert_eq!(symbol_at(&compact, at + 16), personality);
                // DWARF32 header, sdata4 start/range, then the one-byte augmentation
                // length: the LSDA pointer starts 17 bytes into this FDE.
                assert_eq!(symbol_at(&eh, fde.offset() as u64 + 17), symbol_at(&compact, at + 24));
            }
            index += 1;
        }
        assert_eq!(index, 5);
    }
}

#[test]
fn compact_encoding_uses_the_current_fde_and_personality_associations() {
    assert_eq!(encoding(Some(0x5300_1234), 0x0300_0000, 0x5678, false).unwrap(), 0x0300_5678);
    assert_eq!(encoding(Some(0x3400_0103), 0x0300_0000, 0, true).unwrap(), 0x4400_0103);
    assert_eq!(encoding(None, 0x0400_0000, 0xff_ffff, false).unwrap(), 0x04ff_ffff);
    assert!(encoding(None, 0x0300_0000, 0x100_0000, false).is_err());
    // A native encoding needs no FDE offset in its 24 bits.
    assert_eq!(encoding(Some(0x0400_0000), 0x0300_0000, 0x100_0000, false).unwrap(), 0x0400_0000);
}

#[test]
fn elf_keeps_dwarf_without_a_compact_section() {
    let mut module = module("x86_64-unknown-linux-gnu");
    let mut tables = UnwindContext::new(&mut module).unwrap();
    let (id, unwind) = function(&mut module, "frame");
    tables.add_function(&mut module, id, &unwind).unwrap();
    tables.name_personality(&mut module).unwrap();
    let mut product = module.finish();
    tables.emit(&mut product).unwrap();
    let bytes = product.emit().unwrap();
    let file = object::File::parse(bytes.as_slice()).unwrap();
    assert!(file.section_by_name(".eh_frame").is_some());
    assert!(file.section_by_name("__compact_unwind").is_none());
}
