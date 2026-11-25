//! The Rust-record ELF shared-object writer, read back by an independent byte parser here (the
//! host test is macOS's; the loader proof is Linux arm64's). The checks quote the System V gABI
//! (`Elf64_Ehdr`, `Elf64_Phdr`, `Elf64_Shdr`, `Elf64_Sym`, `Elf64_Rela`, `Elf64_Dyn`, SysV hash),
//! glibc's `<elf.h>` program/dynamic tags, and the AArch64 ABI relocation numbers.

use super::{alloc, cstr, u16_at, u32_at, u64_at};
use blaze_code::record::Symbol;
use blaze_export::eh_out::EhSections;
use blaze_export::elf;
use blaze_export::input::{CExport, Platform};
use blaze_export::layout::{FixTo, Fixup, Import, Laid, Placed};
use blaze_own::{AVec, Vec};

const CONST_AT: u64 = 0x1_0000;
const DATA_AT: u64 = 0x2_0000;
const END: u64 = 0x3_0000;
const RET: u32 = 0xd65f_03c0;
const MOV_W0_42: u32 = 0x5280_0540;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;
const PT_GNU_EH_FRAME: u32 = 0x6474_E550;
const PT_GNU_STACK: u32 = 0x6474_E551;
const PT_GNU_RELRO: u32 = 0x6474_E552;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const DT_NEEDED: u64 = 1;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_INIT: u64 = 12;
const DT_SONAME: u64 = 14;
const DT_INIT_ARRAY: u64 = 25;
const DT_FLAGS: u64 = 30;
const DT_RELACOUNT: u64 = 0x6FFF_FFF9;
const DT_FLAGS_1: u64 = 0x6FFF_FFFB;
const DF_BIND_NOW: u64 = 0x8;
const DF_STATIC_TLS: u64 = 0x10;
const DF_1_NOW: u64 = 0x1;
const DF_1_PIE: u64 = 0x0800_0000;

const R_AARCH64_GLOB_DAT: u64 = 1025;
const R_AARCH64_RELATIVE: u64 = 1027;
const R_AARCH64_TLS_TPREL64: u64 = 1030;

const SHT_PROGBITS: u32 = 1;
const SHF_ALLOC: u64 = 2;
const SHF_EXECINSTR: u64 = 4;

#[derive(Clone, Copy)]
struct Phdr {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

fn phdrs(file: &[u8]) -> AVec<'static, Phdr> {
    let phoff = u64_at(file, 32) as usize;
    let phnum = u16_at(file, 56) as usize;
    let mut out: AVec<'static, Phdr> = Vec::new_in(alloc());
    for k in 0..phnum {
        let at = phoff + 56 * k;
        out.push(Phdr {
            kind: u32_at(file, at),
            flags: u32_at(file, at + 4),
            offset: u64_at(file, at + 8),
            vaddr: u64_at(file, at + 16),
            filesz: u64_at(file, at + 32),
            memsz: u64_at(file, at + 40),
            align: u64_at(file, at + 48),
        });
    }
    out
}

fn file_at(phdrs: &[Phdr], vaddr: u64) -> usize {
    let p = phdrs
        .iter()
        .find(|p| p.kind == PT_LOAD && vaddr >= p.vaddr && vaddr < p.vaddr + p.filesz)
        .unwrap_or_else(|| panic!("virtual address {vaddr:#x} is in no load's file bytes"));
    (p.offset + vaddr - p.vaddr) as usize
}

fn dynamic(file: &[u8], phdrs: &[Phdr]) -> AVec<'static, (u64, u64)> {
    let p = phdrs.iter().find(|p| p.kind == PT_DYNAMIC).expect("PT_DYNAMIC");
    let mut out: AVec<'static, (u64, u64)> = Vec::new_in(alloc());
    for k in 0..p.filesz / 16 {
        let at = p.offset as usize + 16 * k as usize;
        let tag = u64_at(file, at);
        let value = u64_at(file, at + 8);
        out.push((tag, value));
    }
    out
}

fn tag<'a>(dynamic: &'a [(u64, u64)], tag: u64) -> Option<&'a (u64, u64)> {
    dynamic.iter().find(|&&(t, _)| t == tag)
}

#[derive(Clone, Copy)]
struct Shdr {
    name: u32,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
}

fn sections(file: &[u8]) -> (AVec<'static, Shdr>, usize) {
    let shoff = u64_at(file, 40) as usize;
    let shnum = u16_at(file, 60) as usize;
    let shstrndx = u16_at(file, 62) as usize;
    let mut out: AVec<'static, Shdr> = Vec::new_in(alloc());
    for k in 0..shnum {
        let at = shoff + 64 * k;
        out.push(Shdr {
            name: u32_at(file, at),
            kind: u32_at(file, at + 4),
            flags: u64_at(file, at + 8),
            addr: u64_at(file, at + 16),
            offset: u64_at(file, at + 24),
            size: u64_at(file, at + 32),
            link: u32_at(file, at + 40),
            info: u32_at(file, at + 44),
            align: u64_at(file, at + 48),
            entsize: u64_at(file, at + 56),
        });
    }
    (out, shstrndx)
}

fn elf_hash(name: &[u8]) -> u32 {
    let mut h = 0u32;
    for &c in name {
        h = (h << 4).wrapping_add(u32::from(c));
        let g = h & 0xF000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

struct Fixture {
    file: AVec<'static, u8>,
    id: [u8; 16],
    text_at: u64,
    init_at: u64,
    malloc_at: u64,
    desc_at: u64,
    data_word_at: u64,
    mod_init_at: u64,
}

/// One exported function returning 42 at text_at, one TLS initializer after it, one `malloc`
/// GOT import, a writable data word rebased to the exported function, and (for `tls`) the
/// 40-byte descriptor at the start of data. The byte content is deliberately small; the test is
/// of ELF structure, with the export also callable by the Linux loader proof.
fn fixture(tls: bool) -> Fixture {
    let a = alloc();
    let mut text: AVec<'static, u8> = Vec::new_in(a);
    for inst in [MOV_W0_42, RET, RET, RET] {
        text.extend_from_slice(&inst.to_le_bytes());
    }
    let text_at = elf::TEXT_AT;
    let init_at = text_at + 8;
    let plan = elf::plan_library_for(None, tls);
    let got_at = CONST_AT + plan.const_head;
    let malloc_at = got_at;
    let mod_init_at = got_at + 8;
    let mut konst: AVec<'static, u8> = Vec::new_in(a);
    konst.resize((plan.const_head + 16) as usize, 0xEE);

    let mut data: AVec<'static, u8> = Vec::new_in(a);
    let desc_at = DATA_AT;
    let data_word_at = if tls { DATA_AT + 40 } else { DATA_AT };
    data.resize(if tls { 48 } else { 8 }, 0xEE);

    let import_symbol = Symbol([1; 32]);
    let export_symbol = Symbol([2; 32]);
    let init_symbol = Symbol([3; 32]);
    let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
    imports.push(Import { symbol: import_symbol, name: "malloc" });
    let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
    fixups.push(Fixup { at: malloc_at, to: FixTo::Bind { import: 0 } });
    if tls {
        fixups.push(Fixup { at: mod_init_at, to: FixTo::Rebase { target: init_at } });
    }
    fixups.push(Fixup { at: data_word_at, to: FixTo::Rebase { target: text_at } });
    fixups.sort_unstable_by_key(|f| f.at);
    let mut placed: AVec<'static, Placed> = Vec::new_in(a);
    placed.push(Placed { symbol: export_symbol, at: text_at, len: 8, tier: None, folded: false });
    placed.push(Placed { symbol: init_symbol, at: init_at, len: 4, tier: None, folded: false });

    let laid = Laid {
        page: elf::PAGE,
        text_at,
        text,
        const_at: CONST_AT,
        konst,
        got_at,
        data_at: DATA_AT,
        data,
        zero_at: if tls { DATA_AT + 48 } else { DATA_AT + 8 },
        zero_len: 0,
        end: END,
        imports,
        fixups,
        entry: 0,
        placed,
        tables_room: 0,
        mod_init_at: tls.then_some(mod_init_at),
        has_tls: tls,
        tls_count: u32::from(tls),
        tls_desc_at: tls.then_some(desc_at),
    };
    let mut eh_frame: AVec<'static, u8> = Vec::new_in(a);
    eh_frame.extend_from_slice(&[0, 0, 0, 0]);
    let eh_at = laid.text_end();
    let eh = EhSections {
        platform: Platform::LinuxArm64,
        eh_frame_at: eh_at,
        eh_frame,
        except_tab_at: eh_at + 4,
        except_tab: Vec::new_in(a),
        unwind_info_at: eh_at + 4,
        unwind_info: Vec::new_in(a),
        eh_frame_hdr_at: eh_at + 4,
        eh_frame_hdr: Vec::new_in(a),
        functions: 0,
        landing: 0,
    };
    let exports = [CExport { name: b"exported_len", symbol: export_symbol }];
    let mut file: AVec<'static, u8> = Vec::new_in(a);
    let seed = [7; 32];
    let id = elf::write_library_native(&laid, &eh, None, &exports, "fixture", &seed, &mut file, a).expect("the shared object writes");
    Fixture { file, id, text_at, init_at, malloc_at, desc_at, data_word_at, mod_init_at }
}

fn hash_lookup(file: &[u8], phdrs: &[Phdr], dynamic: &[(u64, u64)], name: &[u8]) -> usize {
    let hash_at = file_at(phdrs, tag(dynamic, DT_HASH).expect("DT_HASH").1);
    let nbucket = u32_at(file, hash_at) as usize;
    let nchain = u32_at(file, hash_at + 4) as usize;
    let buckets = hash_at + 8;
    let chains = buckets + 4 * nbucket;
    let mut row = u32_at(file, buckets + 4 * (elf_hash(name) as usize % nbucket)) as usize;
    let strtab = file_at(phdrs, tag(dynamic, DT_STRTAB).expect("DT_STRTAB").1);
    let symtab = file_at(phdrs, tag(dynamic, DT_SYMTAB).expect("DT_SYMTAB").1);
    for _ in 0..nchain {
        assert_ne!(row, 0, "{name:?} is absent from the SysV hash chain");
        assert!(row < nchain, "SysV hash row {row} past nchain {nchain}");
        let name_at = u32_at(file, symtab + 24 * row) as usize;
        if cstr(file, strtab + name_at) == name {
            return row;
        }
        row = u32_at(file, chains + 4 * row) as usize;
    }
    panic!("SysV hash chain does not terminate or find {name:?}");
}

#[test]
fn rust_record_library_exports_symbols_and_relocates_tls() {
    let f = fixture(true);
    let file = &f.file[..];
    assert_eq!(&file[..4], b"\x7fELF");
    assert_eq!(u16_at(file, 16), 3, "ET_DYN");
    assert_eq!(u16_at(file, 18), 183, "EM_AARCH64");
    assert_eq!(u64_at(file, 24), 0, "a library has no entry");
    assert_ne!(u64_at(file, 40), 0, "defined dynamic symbols need section headers");
    assert_eq!(u16_at(file, 60), 8, "the writer's eight sections");
    assert_eq!(u16_at(file, 62), 7, ".shstrtab is section 7");

    let ph = phdrs(file);
    assert!(!ph.iter().any(|p| p.kind == PT_INTERP), "a dlopened library has no interpreter");
    assert!(ph.iter().any(|p| p.kind == PT_GNU_EH_FRAME), ".eh_frame_hdr is exposed to the unwinder");
    assert!(ph.iter().any(|p| p.kind == PT_GNU_RELRO), "the dynamic/GOT head is RELRO");
    let stack = ph.iter().find(|p| p.kind == PT_GNU_STACK).expect("PT_GNU_STACK");
    assert_eq!(stack.flags & PF_X, 0, "the stack is non-executable");
    assert_eq!(stack.flags & (PF_R | PF_W), PF_R | PF_W);
    let tls = ph.iter().find(|p| p.kind == PT_TLS).expect("PT_TLS for Rust ROOT");
    assert_eq!((tls.filesz, tls.memsz, tls.align), (0, 8, 16), "one zeroed 8-byte static-TLS root");
    assert_eq!(tls.offset % tls.align, tls.vaddr % tls.align, "PT_TLS offset/address congruence");

    let dyn_entries = dynamic(file, &ph);
    let flags = tag(&dyn_entries, DT_FLAGS).expect("DT_FLAGS").1;
    assert_eq!(flags & DF_BIND_NOW, DF_BIND_NOW, "eager binding for RELRO");
    assert_eq!(flags & DF_STATIC_TLS, DF_STATIC_TLS, "TPREL TLS is static TLS");
    let flags1 = tag(&dyn_entries, DT_FLAGS_1).expect("DT_FLAGS_1").1;
    assert_eq!(flags1 & DF_1_NOW, DF_1_NOW);
    assert_eq!(flags1 & DF_1_PIE, 0, "a shared object is not a PIE executable");
    assert!(tag(&dyn_entries, DT_NEEDED).is_some(), "libc is needed");
    assert_eq!(tag(&dyn_entries, DT_INIT).expect("DT_INIT").1, f.init_at, "Rust TLS init runs before constructors");
    assert!(tag(&dyn_entries, DT_INIT_ARRAY).is_none(), "no native image means no constructor array");
    let strtab = file_at(&ph, tag(&dyn_entries, DT_STRTAB).expect("DT_STRTAB").1);
    let soname_at = tag(&dyn_entries, DT_SONAME).expect("DT_SONAME").1 as usize;
    assert_eq!(cstr(file, strtab + soname_at), b"libfixture.so");

    let (sections, shstrndx) = sections(file);
    let shstr = sections[shstrndx];
    let mut text_index = None;
    for (k, s) in sections.iter().enumerate() {
        let name = cstr(file, shstr.offset as usize + s.name as usize);
        if name == b".text" {
            text_index = Some(k);
            assert_eq!(s.kind, SHT_PROGBITS);
            assert_eq!(s.flags & (SHF_ALLOC | SHF_EXECINSTR), SHF_ALLOC | SHF_EXECINSTR, ".text is allocated executable");
            assert_eq!(s.addr, f.text_at);
            assert_eq!(s.size, 16, "the fixture's four instructions");
            assert_eq!(s.align, 16);
        } else if name == b".dynsym" {
            assert_eq!(k, 3, "the writer's fixed section order");
            assert_eq!(s.info, 1, "every fixture dynsym is global from row 1");
            assert_eq!((s.align, s.entsize), (8, 24));
        } else if name == b".dynstr" {
            assert_eq!(k, 4, "the writer's fixed section order");
            assert_eq!(s.align, 1);
        } else if name == b".dynamic" {
            assert_eq!(s.link, 4, ".dynamic links .dynstr");
            assert_eq!((s.align, s.entsize), (8, 16));
        } else if name == b".hash" {
            assert_eq!(s.link, 3, ".hash links .dynsym");
            assert_eq!((s.align, s.entsize), (4, 4));
        } else if name == b".rela.dyn" {
            assert_eq!(s.link, 3, ".rela.dyn links .dynsym");
            assert_eq!((s.align, s.entsize), (8, 24));
        }
    }
    let text_index = text_index.expect(".text section");
    let row = hash_lookup(file, &ph, &dyn_entries, b"exported_len");
    let symtab = file_at(&ph, tag(&dyn_entries, DT_SYMTAB).expect("DT_SYMTAB").1);
    let sym = symtab + 24 * row;
    let export_name_at = u32_at(file, sym) as usize;
    assert_eq!(cstr(file, strtab + export_name_at), b"exported_len");
    assert_eq!(file[sym + 4] >> 4, 1, "STB_GLOBAL");
    assert_eq!(file[sym + 4] & 0xF, 2, "STT_FUNC");
    assert_eq!(u16_at(file, sym + 6) as usize, text_index, "the export is defined in .text");
    assert_eq!(u64_at(file, sym + 8), f.text_at, "the export value is its image offset");
    assert_eq!(u64_at(file, sym + 16), 8, "the export size is the function length");

    let rela_at = file_at(&ph, tag(&dyn_entries, DT_RELA).expect("DT_RELA").1);
    let rela_len = tag(&dyn_entries, DT_RELASZ).expect("DT_RELASZ").1 as usize;
    let relacount = tag(&dyn_entries, DT_RELACOUNT).expect("DT_RELACOUNT").1 as usize;
    let mut saw_got = false;
    let mut saw_init = false;
    let mut saw_data = false;
    let mut saw_root = false;
    for k in 0..rela_len / 24 {
        let at = rela_at + 24 * k;
        let r_offset = u64_at(file, at);
        let r_info = u64_at(file, at + 8);
        let r_addend = u64_at(file, at + 16);
        let kind = r_info & 0xFFFF_FFFF;
        if k < relacount {
            assert_eq!(kind, R_AARCH64_RELATIVE, "the first DT_RELACOUNT relocations are relative");
        } else {
            assert_ne!(kind, R_AARCH64_RELATIVE, "relative relocations do not follow symbolic ones");
        }
        if r_offset == f.malloc_at {
            saw_got = true;
            assert_eq!(kind, R_AARCH64_GLOB_DAT, "a GOT import binds lazily through GLOB_DAT semantics");
        }
        if r_offset == f.mod_init_at {
            saw_init = true;
            assert_eq!((kind, r_addend), (R_AARCH64_RELATIVE, f.init_at));
        }
        if r_offset == f.data_word_at {
            saw_data = true;
            assert_eq!((kind, r_addend), (R_AARCH64_RELATIVE, f.text_at));
        }
        if r_offset == f.desc_at + 8 {
            saw_root = true;
            assert_eq!(r_info, R_AARCH64_TLS_TPREL64, "local TPREL relocation, symbol 0");
            assert_eq!(r_addend, 0, "ROOT is at offset 0 of the Rust-only TLS template");
        }
    }
    assert!(saw_got && saw_init && saw_data && saw_root, "GOT, init, data and TLS relocations all present");
    let root_file = f.desc_at as usize + 8;
    assert_eq!(&file[root_file..root_file + 8], &[0; 8], "RELA leaves the relocated root cell zero");

    let again = fixture(true);
    assert_eq!(f.file, again.file, "shared-object bytes are deterministic");
    assert_eq!(f.id, again.id, "the content build id is deterministic");
}

#[test]
fn rust_record_library_without_tls_has_no_tls_or_initializer_entries() {
    let f = fixture(false);
    let file = &f.file[..];
    let ph = phdrs(file);
    assert!(!ph.iter().any(|p| p.kind == PT_TLS));
    let dyn_entries = dynamic(file, &ph);
    assert!(tag(&dyn_entries, DT_INIT).is_none());
    assert_eq!(tag(&dyn_entries, DT_FLAGS).expect("DT_FLAGS").1 & DF_STATIC_TLS, 0);
    assert_eq!(tag(&dyn_entries, DT_FLAGS_1).expect("DT_FLAGS_1").1 & DF_1_PIE, 0);
    let row = hash_lookup(file, &ph, &dyn_entries, b"exported_len");
    let symtab = file_at(&ph, tag(&dyn_entries, DT_SYMTAB).expect("DT_SYMTAB").1);
    assert_eq!(u64_at(file, symtab + 24 * row + 8), f.text_at);
}
