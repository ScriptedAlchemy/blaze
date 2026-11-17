//! Call frame records' 32-bit fields in an ELF image past 2 GiB (`crate::elf`'s layout): an
//! AArch64 dev image with 1 GiB of code keeps a patch room as large after it, so the code and
//! its room span more than 2 GiB. A CIE's personality (`indirect|pcrel|sdata4`) reaches its
//! GOT cell and an FDE's LSDA (`pcrel|sdata4`) its data all the same: relro, with the GOT, is
//! laid out between `.eh_frame` and the code, where it was after the room, 0x9d3dc2c1 bytes
//! from `.eh_frame` in blaze's own debug build. Every field is read back with gimli.

use blaze_codegen::{Arch, CompiledFunction, Data, Target, Unwind};
use gimli::{BaseAddresses, CieOrFde, EhFrame, LittleEndian, Pointer, UnwindSection as _};
use object::Object as _;

use super::{Change, Disk, Image, Linked, TO, function, id, key, name, output, section_of};
use crate::Output;

/// A one-byte retained native section with a large address alignment: reproduces the
/// personality gap without allocating 1 GiB of code. ET_REL's sh_addr is zero; the section
/// bytes need no corresponding file padding. Only sh_addralign differs between controls.
fn native_gap(align: u64) -> Vec<u8> {
    use object::write::Object as Builder;
    use object::{
        Architecture, BinaryFormat, Endianness, ObjectSection as _, SectionFlags, SectionKind, elf,
    };

    let mut object = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let gap = object.add_section(Vec::new(), b".blaze.probe_gap".to_vec(), SectionKind::Text);
    object.append_section_data(gap, &[0xc3], 1);
    object.section_mut(gap).flags = SectionFlags::Elf {
        sh_type: elf::SHT_PROGBITS,
        sh_flags: elf::SHF_ALLOC | elf::SHF_EXECINSTR | elf::SHF_GNU_RETAIN,
    };
    let mut bytes = object.write().unwrap();
    let index = object::File::parse(&bytes[..])
        .unwrap()
        .section_by_name(".blaze.probe_gap")
        .unwrap()
        .index()
        .0;
    let shoff = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    let at = shoff + index * 64 + 48;
    bytes[at..at + 8].copy_from_slice(&align.to_le_bytes());
    bytes
}

#[test]
fn a_native_alignment_gap_keeps_the_personality_in_reach_without_large_code() {
    use std::sync::Arc;

    use super::{Native, NativeKind};
    use crate::model::Class;

    for align in [16, 1 << 31] {
        let mut start = function(1, &[0xc3], Vec::new());
        start.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            // DW_CFA_def_cfa rsp, 8; DW_CFA_offset rip, -8.
            cie: vec![0x0c, 7, 8, 0x90, 1],
            fde: Vec::new(),
            personality: Some(Target::Direct(key(3))),
            lsda: None,
        });
        let personality = function(3, &[0xc3], Vec::new());
        let native: Arc<[u8]> = native_gap(align).into();
        let mut image = Image::new(name(TO), output(true)).unwrap();
        let change = Change {
            added: vec![&start, &personality],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![(key(3), "rust_eh_personality".into())],
            natives: vec![Native {
                id: id(100),
                name: "personality-gap.o",
                bytes: &native,
                kind: NativeKind::Object,
                order: 0,
            }],
            ..Change::default()
        };
        let staged = image
            .stage(change)
            .unwrap_or_else(|error| panic!("native alignment {align:#x}: {error}"));
        // Inspect the real relocated regions, without materializing a >2 GiB file.
        let space = |class: &Class| {
            staged.state.spaces.iter().find(|it| &it.region.class == class).unwrap()
        };
        let (eh, got, text) = (space(&Class::EhFrame), space(&Class::Got), space(&Class::Text));
        let gap = staged
            .state
            .spaces
            .iter()
            .find(|it| matches!(
                &it.region.class, Class::Named { name, .. } if name.as_str() == ".blaze.probe_gap"
            ))
            .expect("SHF_GNU_RETAIN keeps the native gap");
        assert_eq!(gap.region.size, 1);
        assert_eq!(gap.region.addr % align, 0);
        if align == 1 << 31 {
            assert!(gap.region.addr >= 1 << 31);
            assert!(staged.len() > i32::MAX as u64);
            assert!(eh.region.addr < got.region.addr && got.region.addr < text.region.addr);
        }
        let bytes = eh.bytes.read(0, eh.region.size as usize);
        let bases = BaseAddresses::default().set_eh_frame(eh.region.addr);
        let section = EhFrame::new(&bytes, LittleEndian);
        let mut entries = section.entries(&bases);
        let (mut cells, mut fdes) = (Vec::new(), Vec::new());
        while let Some(entry) = entries.next().unwrap() {
            match entry {
                CieOrFde::Cie(cie) => cells.extend(cie.personality()),
                CieOrFde::Fde(partial) => {
                    let fde = partial.parse(EhFrame::cie_from_offset).unwrap();
                    fdes.push(fde.initial_address());
                }
            }
        }
        let [Pointer::Indirect(cell)] = cells[..] else { panic!("{cells:?}") };
        assert!((got.region.addr..got.region.addr + got.region.size).contains(&cell));
        assert_eq!(fdes.len(), 1);
        assert!((text.region.addr..text.region.addr + text.region.size).contains(&fdes[0]));
        let rela = space(&Class::Synthetic(".rela.dyn"));
        let bytes = rela.bytes.read(0, rela.region.size as usize);
        let targets: Vec<u64> = bytes
            .chunks_exact(24)
            .filter_map(|entry| {
                let at = u64::from_le_bytes(entry[..8].try_into().unwrap());
                (at == cell).then(|| u64::from_le_bytes(entry[16..24].try_into().unwrap()))
            })
            .collect();
        assert_eq!(targets.len(), 1, "the loader relocates the personality cell once");
        assert!((text.region.addr..text.region.addr + text.region.size).contains(&targets[0]));
    }
}

const GIB: u64 = 1 << 30;
/// `ret`.
const RET: [u8; 4] = [0xc0, 0x03, 0x5f, 0xd6];

fn a64() -> Output {
    Output {
        arch: Arch::Aarch64,
        interpreter: Some("/lib/ld-linux-aarch64.so.1".into()),
        triple: "aarch64-unknown-linux-gnu".into(),
        ..output(true)
    }
}

fn a64_function(n: u8, code: &[u8]) -> CompiledFunction {
    CompiledFunction { arch: Arch::Aarch64, ..function(n, code, Vec::new()) }
}

/// `ret`, with a frame whose CIE names the personality `personality` and whose FDE names its
/// LSDA, one of its data, as rustc's code with landing pads has them.
fn framed(n: u8, personality: Target) -> CompiledFunction {
    let mut it = a64_function(n, &RET);
    let lsda = id(70);
    it.data = vec![Data {
        id: lsda,
        align: 4,
        bytes: vec![0xff, 0x9b, 0x01, 0x00],
        relocs: Default::default(),
        writable: false,
    }];
    it.unwind = Some(Unwind::Dwarf {
        code_align: 4,
        data_align: -8,
        return_address: 30,
        // DW_CFA_def_cfa sp, 0
        cie: vec![0x0c, 31, 0],
        fde: Vec::new(),
        personality: Some(personality),
        lsda: Some(lsda),
    });
    it
}

/// 1 GiB of code ending in `ret`: the most room a patch region keeps (`place::patch_room`)
/// follows it, so the code and the room pass 2 GiB. Its bytes are zeroed pages nothing
/// touches but the last: the test disk keeps no page of zeros it has not got.
fn filler(n: u8) -> CompiledFunction {
    let mut it = a64_function(n, &RET);
    let mut code = vec![0u8; GIB as usize];
    let end = code.len() - 4;
    code[end..].copy_from_slice(&RET);
    it.code = code;
    it
}

#[test]
fn a_personality_and_an_lsda_reach_from_eh_frame_past_2_gib_of_code_and_patch_room() {
    let (start, big, personality) =
        (framed(1, Target::Direct(key(3))), filler(2), a64_function(3, &RET));
    let mut image = Image::new(name(TO), a64()).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start, &big, &personality],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![
            (key(1), "start".into()),
            (key(2), "big".into()),
            (key(3), "rust_eh_personality".into()),
        ],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap_or_else(|it| panic!("{it}"));
    let linked = Linked::of(disk.file(TO));

    let (eh_frame, eh_len) = section_of(&linked, ".eh_frame");
    let (got, got_len) = section_of(&linked, ".got");
    let (patch, _) = section_of(&linked, ".blaze.patch");
    // The layout this is about: past the code, its patch room ends more than 2 GiB from
    // `.eh_frame`, so a GOT after it would be out of a 32-bit field's reach.
    assert!(
        patch + GIB - eh_frame > i32::MAX as u64,
        "the code and its room, {eh_frame:#x}..{:#x}, span 2 GiB",
        patch + GIB
    );
    assert!(eh_frame < got && got < linked.symbol("start"), "relro between .eh_frame and the code");

    let bases = BaseAddresses::default().set_eh_frame(eh_frame);
    let section = EhFrame::new(linked.at(eh_frame, eh_len as usize), LittleEndian);
    let mut entries = section.entries(&bases);
    let (mut cells, mut fdes) = (Vec::new(), Vec::new());
    while let Some(entry) = entries.next().unwrap() {
        match entry {
            CieOrFde::Cie(cie) => cells.extend(cie.personality()),
            CieOrFde::Fde(partial) => {
                let fde = partial.parse(EhFrame::cie_from_offset).unwrap();
                fdes.push((fde.initial_address(), fde.lsda()));
            }
        }
    }
    let [Pointer::Indirect(cell)] = cells[..] else {
        panic!("one CIE with an indirect personality: {cells:?}")
    };
    assert!((got..got + got_len).contains(&cell), "the personality's cell {cell:#x} is in .got");
    let elf = linked.elf();
    let relocated: Vec<i64> = elf
        .dynamic_relocations()
        .expect("dynamic relocations")
        .filter(|(at, _)| *at == cell)
        .map(|(_, it)| it.addend())
        .collect();
    assert_eq!(
        relocated,
        [linked.symbol("rust_eh_personality") as i64],
        "the loader puts the personality's address in its cell"
    );
    let [(covers, Some(Pointer::Direct(lsda)))] = fdes[..] else {
        panic!("one FDE with a direct LSDA: {fdes:?}")
    };
    assert_eq!(covers, linked.symbol("start"), "the FDE covers `start`");
    assert_eq!(linked.at(lsda, 4), [0xff, 0x9b, 0x01, 0x00], "the FDE's LSDA is start's data");
}
