//! The ELF writer: a laid-out program as one AArch64 Linux executable, position-independent
//! (`ET_DYN` with `DF_1_PIE`), started by glibc's dynamic loader, which binds its C names and
//! applies its relocations before `_start`. Structures: the System V gABI and its AArch64
//! supplement (`Elf64_Ehdr`, `Elf64_Phdr`, `Elf64_Dyn`, `Elf64_Sym`, `Elf64_Rela`, the SysV hash
//! table; `R_AARCH64_RELATIVE` 1027, `R_AARCH64_GLOB_DAT` 1025, `R_AARCH64_ABS64` 257), and
//! glibc's `<elf.h>` for the GNU program headers and `DT_FLAGS_1`.
//!
//! The file has no section headers: a loader reads program headers only.
//!
//! | load | holds | protection |
//! |---|---|---|
//! | text | the ELF header, program headers, the interpreter's path, the build-id note, the code | r-x |
//! | const | the dynamic section, the GOT, relro parts, read-only data (`PT_GNU_RELRO` over all of it) | rw-, then r-- |
//! | data | writable statics, zeroed ones past the file bytes | rw- |
//! | meta | `.dynsym`, `.dynstr`, the SysV hash, `.rela.dyn` | r-- |
//!
//! Facts marked "to verify" (by the Linux run test, not written yet: the host test is macOS's):
//! glibc's `ld.so` accepts the dynamic section's tables in a load after the data; an
//! unversioned reference binds to glibc's default version of the name; a one-bucket SysV hash
//! with every chain empty is a valid table for an executable that exports nothing.

use blaze_code::record::{Space, Target};
use blaze_ids::Hasher;
use blaze_native::image::{Image, Region};
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::bytes;
use crate::eh_out::EhSections;
use crate::error::ExportError;
use crate::input::CExport;
use crate::layout::{FixTo, Laid, Plan};
use crate::native_merge::{ElfRelaKind, MergeError, MergedElf};

pub const PAGE: u64 = 0x1_0000;

pub(crate) const EHDR: u64 = 64;
pub(crate) const PHDR: u64 = 56;
/// Every program header at the most: PHDR, INTERP, NOTE, up to six LOADs (text, const, data,
/// native text, native data, meta), DYNAMIC, TLS, GNU_RELRO, GNU_EH_FRAME, GNU_STACK.
const MAX_PHDRS: u64 = 14;
const INTERP: &str = "/lib/ld-linux-aarch64.so.1";
const INTERP_AT: u64 = EHDR + PHDR * MAX_PHDRS;
const NOTE_AT: u64 = (INTERP_AT + INTERP.len() as u64 + 1 + 3) & !3;
/// `Elf64_Nhdr` (namesz, descsz, type), "GNU\0", a 16-byte id.
const NOTE_LEN: u64 = 12 + 4 + 16;
pub const TEXT_AT: u64 = (NOTE_AT + NOTE_LEN + 15) & !15;

pub(crate) const PT_LOAD: u32 = 1;
pub(crate) const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_PHDR: u32 = 6;
pub(crate) const PT_TLS: u32 = 7;
/// AArch64 ELF TLS Variant I layout for the main executable's per-thread `ROOT` pointer:
/// TCB is 16 bytes at `tpidr_el0`, so with `p_align = 16` and `p_vaddr % p_align == 0`, both
/// glibc and musl place byte 0 of `PT_TLS` at `tpidr_el0 + align_up(16, 16) = tpidr_el0 + 16`.
pub(crate) const TLS_ROOT_ALIGN: u64 = 16;
pub(crate) const TLS_ROOT_OFFSET: u64 = 16;
pub(crate) const TLS_ROOT_MEMSZ: u64 = 8;
/// `.eh_frame_hdr`, which glibc's and libgcc's unwinders find through `dl_iterate_phdr`.
pub(crate) const PT_GNU_EH_FRAME: u32 = 0x6474_E550;
pub(crate) const PT_GNU_STACK: u32 = 0x6474_E551;
pub(crate) const PT_GNU_RELRO: u32 = 0x6474_E552;
pub(crate) const PF_X: u32 = 1;
pub(crate) const PF_W: u32 = 2;
pub(crate) const PF_R: u32 = 4;
const NT_GNU_BUILD_ID: u32 = 3;

pub(crate) const DT_NULL: u64 = 0;
pub(crate) const DT_NEEDED: u64 = 1;
const DT_HASH: u64 = 4;
pub(crate) const DT_STRTAB: u64 = 5;
pub(crate) const DT_SYMTAB: u64 = 6;
pub(crate) const DT_RELA: u64 = 7;
pub(crate) const DT_RELASZ: u64 = 8;
pub(crate) const DT_RELAENT: u64 = 9;
pub(crate) const DT_STRSZ: u64 = 10;
pub(crate) const DT_SYMENT: u64 = 11;
const DT_INIT: u64 = 12;
const DT_SONAME: u64 = 14;
const DT_DEBUG: u64 = 21;
const DT_INIT_ARRAY: u64 = 25;
const DT_INIT_ARRAYSZ: u64 = 27;
pub(crate) const DT_FLAGS: u64 = 30;
pub(crate) const DT_RELACOUNT: u64 = 0x6FFF_FFF9;
pub(crate) const DT_FLAGS_1: u64 = 0x6FFF_FFFB;
pub(crate) const DF_BIND_NOW: u64 = 0x8;
pub(crate) const DF_STATIC_TLS: u64 = 0x10;
pub(crate) const DF_1_NOW: u64 = 0x1;
const DF_1_PIE: u64 = 0x0800_0000;
/// `DF_1_NOW | DF_1_PIE`.
const DF_1: u64 = DF_1_NOW | DF_1_PIE;
/// The base dynamic section's entries (excluding extra native `DT_NEEDED` and `DT_INIT_ARRAY`).
const DYN_COUNT: u64 = 14;
/// Room for auto-added standard Linux libraries (`libgcc_s.so.1`, `libm.so.6`) when program
/// imports reference `_Unwind_*` or libm math functions without a native image already listing them.
const AUTO_NEEDED_ROOM: u64 = 2;
/// 16-byte reserved tail in `const_head` right before the GOT holding the self-referential
/// 8-byte `__dso_handle` word (at `got_at - 8`, relocated via `R_AARCH64_RELATIVE` so both
/// `&__dso_handle` and `*(&__dso_handle)` equal `load_bias + dso_handle_at`).
const DSO_HANDLE_ROOM: u64 = 16;

pub(crate) const R_AARCH64_ABS64: u64 = 257;
pub(crate) const R_AARCH64_GLOB_DAT: u64 = 1025;
pub(crate) const R_AARCH64_RELATIVE: u64 = 1027;
const R_AARCH64_TLS_TPREL64: u64 = 1030;
const R_AARCH64_TLSDESC: u64 = 1031;
/// `STB_GLOBAL << 4 | STT_FUNC`.
const GLOBAL_FUNC: u8 = 0x12;
pub(crate) const STB_GLOBAL: u8 = 1;
pub(crate) const STB_WEAK: u8 = 2;
pub(crate) const STT_NOTYPE: u8 = 0;
pub(crate) const STT_FUNC: u8 = 2;

pub(crate) const SHT_NULL: u32 = 0;
pub(crate) const SHT_PROGBITS: u32 = 1;
pub(crate) const SHT_STRTAB: u32 = 3;
pub(crate) const SHT_RELA: u32 = 4;
pub(crate) const SHT_HASH: u32 = 5;
pub(crate) const SHT_DYNAMIC: u32 = 6;
pub(crate) const SHT_DYNSYM: u32 = 11;
pub(crate) const SHF_WRITE: u64 = 1;
pub(crate) const SHF_ALLOC: u64 = 2;
pub(crate) const SHF_EXECINSTR: u64 = 4;

pub(crate) const LIBC: &str = "libc.so.6";
const LIBGCC_S: &str = "libgcc_s.so.1";
const LIBM: &str = "libm.so.6";
const DSO_HANDLE: &str = "__dso_handle";
const ID_DOMAIN: &str = "blaze-export elf build id 1";

/// Returns true if `name` is a standard C math symbol provided by `libm.so.6` on glibc Linux.
/// Leading marker placed on `Space::Outside` names declared with `#[linkage = "extern_weak"]`.
const WEAK_MARK: char = '\x02';

fn split_weak(name: &str) -> (&str, bool) {
    match name.strip_prefix(WEAK_MARK) {
        Some(real) => (real, true),
        None => (name, false),
    }
}

fn is_libm_symbol(name: &str) -> bool {
    matches!(
        name,
        "acos" | "acosf" | "acosh" | "acoshf"
        | "asin" | "asinf" | "asinh" | "asinhf"
        | "atan" | "atanf" | "atan2" | "atan2f" | "atanh" | "atanhf"
        | "cbrt" | "cbrtf" | "ceil" | "ceilf" | "copysign" | "copysignf"
        | "cos" | "cosf" | "cosh" | "coshf"
        | "erf" | "erff" | "erfc" | "erfcf"
        | "exp" | "expf" | "exp2" | "exp2f" | "expm1" | "expm1f"
        | "fabs" | "fabsf" | "fdim" | "fdimf" | "floor" | "floorf"
        | "fma" | "fmaf" | "fmax" | "fmaxf" | "fmin" | "fminf" | "fmod" | "fmodf"
        | "frexp" | "frexpf" | "hypot" | "hypotf" | "ilogb" | "ilogbf"
        | "ldexp" | "ldexpf" | "lgamma" | "lgammaf" | "lgamma_r" | "lgammaf_r"
        | "log" | "logf" | "log10" | "log10f" | "log1p" | "log1pf" | "log2" | "log2f" | "logb" | "logbf"
        | "modf" | "modff" | "nan" | "nanf" | "nearbyint" | "nearbyintf" | "nextafter" | "nextafterf"
        | "pow" | "powf" | "remainder" | "remainderf" | "remquo" | "remquof"
        | "rint" | "rintf" | "round" | "roundf" | "roundeven" | "roundevenf"
        | "scalbn" | "scalbnf" | "sin" | "sinf" | "sincos" | "sincosf" | "sinh" | "sinhf"
        | "sqrt" | "sqrtf" | "tan" | "tanf" | "tanh" | "tanhf" | "tgamma" | "tgammaf"
        | "trunc" | "truncf"
    )
}

/// The layout plan an ELF file with no native image needs: the dynamic section heads the const region.
pub fn plan() -> Plan {
    plan_for(None, false)
}

/// The layout plan for an ELF file with an optional merged native image: reserves room at the head
/// of the const region for every `DT_NEEDED` soname, optional `DT_INIT_ARRAY` / `DT_INIT_ARRAYSZ`,
/// and the self-referential `__dso_handle` word right before the GOT.
pub fn plan_for(native_libraries: Option<&[AString<'_>]>, has_inits: bool) -> Plan {
    let extra_libs = native_libraries.map_or(0, |libs| u64::try_from(libs.len()).unwrap_or(u64::MAX));
    let init_entries = if has_inits { 2u64 } else { 0u64 };
    let dyn_count = DYN_COUNT + extra_libs + AUTO_NEEDED_ROOM + init_entries;
    Plan { page: PAGE, text_at: TEXT_AT, const_head: dyn_count * 16 + DSO_HANDLE_ROOM }
}

/// The layout plan for a loadable ELF shared object. `has_inits` covers either Rust's TLS
/// initializer (`DT_INIT`) or native constructors (`DT_INIT_ARRAY` pair); three entries are
/// reserved so both forms may coexist. The fixed text start intentionally remains [`TEXT_AT`]:
/// shared objects merely leave the interpreter gap unused, keeping one layout rule for both forms.
pub fn plan_library_for(native_libraries: Option<&[AString<'_>]>, has_inits: bool) -> Plan {
    let mut plan = plan_for(native_libraries, has_inits);
    if has_inits {
        plan.const_head += 16; // One DT_INIT may precede the native DT_INIT_ARRAY pair.
    }
    plan
}

/// Computes the `tpidr_el0`-relative byte offset of Rust's 8-byte `ROOT` pointer in the
/// executable's single `PT_TLS` segment (AArch64 Variant I).
///
/// - Without native C TLS: `PT_TLS` holds only Rust's 8-byte `ROOT` with `p_align = 16`, so byte 0
///   sits at `tpidr_el0 + 16` ([`TLS_ROOT_OFFSET`]).
/// - With native C TLS (`c_memsz`, `c_align`): C's `.tdata` + `.tbss` occupy `[0, c_memsz)` of
///   `PT_TLS` (preserving every `R_AARCH64_TLSDESC` / `R_AARCH64_TLS_TPREL64` template offset
///   unchanged), Rust's zero-initialized `ROOT` sits after them in the zero-filled tail at
///   `up(c_memsz, 8)`, and `p_align = c_align.max(16)` with `p_vaddr % p_align == 0`, placing
///   byte 0 at `tpidr_el0 + p_align` and `ROOT` at `tpidr_el0 + p_align + up(c_memsz, 8)`.
pub(crate) fn tls_root_offset(natives: Option<&Image<'_>>) -> Result<u64, MergeError> {
    let Some(image) = natives else {
        return Ok(TLS_ROOT_OFFSET);
    };
    let Some(tdata) = image.region(Region::ThreadData) else {
        return Err(MergeError::NoRegion { region: Region::ThreadData });
    };
    let Some(tbss) = image.region(Region::ThreadZero) else {
        return Err(MergeError::NoRegion { region: Region::ThreadZero });
    };
    let tls_at = if tdata.len > 0 { tdata.at } else { tbss.at };
    let tls_end = if tbss.len > 0 { tbss.at + tbss.len } else { tdata.at + tdata.len };
    let c_memsz = tls_end.saturating_sub(tls_at);
    if c_memsz == 0 {
        return Ok(TLS_ROOT_OFFSET);
    }
    let log2 = tdata.align_log2.max(tbss.align_log2);
    if log2 > 16 {
        return Err(MergeError::TooLarge { what: "a TLS alignment past a 64 KiB page", len: u64::from(log2) });
    }
    let align = (1u64 << log2).max(TLS_ROOT_ALIGN);
    Ok(align + up(c_memsz, 8))
}

fn up(n: u64, to: u64) -> u64 {
    (n + to - 1) & !(to - 1)
}

fn u64_of(n: usize, what: &'static str) -> Result<u64, ExportError> {
    u64::try_from(n).map_err(|_| ExportError::TooLarge { what, len: u64::MAX })
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, ExportError> {
    u32::try_from(n).map_err(|_| ExportError::TooLarge { what, len: u64::try_from(n).unwrap_or(u64::MAX) })
}

fn usize_of(n: u64, what: &'static str) -> Result<usize, ExportError> {
    usize::try_from(n).map_err(|_| ExportError::TooLarge { what, len: n })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn phdr(out: &mut AVec<'_, u8>, kind: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64, align: u64) {
    bytes::u32(out, kind);
    bytes::u32(out, flags);
    bytes::u64(out, offset);
    bytes::u64(out, vaddr);
    bytes::u64(out, vaddr); // p_paddr
    bytes::u64(out, filesz);
    bytes::u64(out, memsz);
    bytes::u64(out, align);
}

#[derive(Clone, Copy)]
struct TlsPhdr {
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

fn intern(table: &mut AVec<'_, u8>, s: &[u8]) -> Result<u32, ExportError> {
    let at = u32_of(table.len(), "a string-table offset")?;
    table.extend_from_slice(s);
    table.push(0);
    Ok(at)
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

fn sym(out: &mut AVec<'_, u8>, name: u32, info: u8, shndx: u16, value: u64, size: u64) {
    bytes::u32(out, name);
    out.push(info);
    out.push(0); // st_other: default visibility
    bytes::u16(out, shndx);
    bytes::u64(out, value);
    bytes::u64(out, size);
}

#[allow(clippy::too_many_arguments)]
fn shdr(
    out: &mut AVec<'_, u8>,
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
) {
    bytes::u32(out, name);
    bytes::u32(out, kind);
    bytes::u64(out, flags);
    bytes::u64(out, addr);
    bytes::u64(out, offset);
    bytes::u64(out, size);
    bytes::u32(out, link);
    bytes::u32(out, info);
    bytes::u64(out, align);
    bytes::u64(out, entsize);
}

/// Writes `laid` as an ELF executable into `out` (its contents replaced). `seed` joins the
/// content in the build id (the profile's digest, or zeros). Returns the build id.
pub fn write<'a>(laid: &Laid<'_, '_>, eh: &EhSections<'_>, seed: &[u8; 32], out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<[u8; 16], ExportError> {
    write_native(laid, eh, None, seed, out, alloc)
}

/// [`write`] with an optional merged native C image ([`MergedElf`]): its `r-x` and `rw-` loads
/// follow the program's loads, its `DT_NEEDED` sonames follow `libc.so.6`, its `Inits` become
/// `DT_INIT_ARRAY`/`DT_INIT_ARRAYSZ`, its relocations join `.rela.dyn`, and its C TLS template
/// coexists with Rust `Space::Tls` in the executable's single `PT_TLS`.
pub fn write_native<'a>(
    laid: &Laid<'_, '_>,
    eh: &EhSections<'_>,
    native: Option<&MergedElf<'_, '_>>,
    seed: &[u8; 32],
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<[u8; 16], ExportError> {
    write_image(laid, eh, native, None, seed, out, alloc)
}

/// Writes `laid` as a genuine `dlopen`-able AArch64 ELF shared object into `out` (its contents
/// replaced), with the C names in `exports` defined in `.dynsym` and the SysV hash. `name` becomes
/// `DT_SONAME` as `lib{name}.so`. Unlike [`write_native`], the file has no `PT_INTERP`, no entry,
/// no `DF_1_PIE`, and it carries section headers because dynamic symbols must name `.text`.
///
/// Rust TLS is initialized by ld.so rather than hard-coding an executable TLS offset: the writer
/// applies `R_AARCH64_TLS_TPREL64` directly to the descriptor's `root` cell. Its addend is the
/// root's offset inside this module's `PT_TLS` template, so ld.so writes the correct TP-relative
/// offset before running `DT_INIT`; the library initializer then sets `ROOT_OWN`.
#[allow(clippy::too_many_arguments)]
pub fn write_library_native<'a>(
    laid: &Laid<'_, '_>,
    eh: &EhSections<'_>,
    native: Option<&MergedElf<'_, '_>>,
    exports: &[CExport<'_>],
    name: &str,
    seed: &[u8; 32],
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<[u8; 16], ExportError> {
    write_image(laid, eh, native, Some(Library { name, exports }), seed, out, alloc)
}

#[derive(Clone, Copy)]
struct Library<'a> {
    name: &'a str,
    exports: &'a [CExport<'a>],
}

/// Serialize the shared ELF load layout. Executables additionally name their interpreter and
/// entry point. Libraries publish dynamic symbols and let the loader initialize Rust TLS.
fn write_image<'a>(
    laid: &Laid<'_, '_>,
    eh: &EhSections<'_>,
    native: Option<&MergedElf<'_, '_>>,
    library: Option<Library<'_>>,
    seed: &[u8; 32],
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<[u8; 16], ExportError> {
    let exports = library.map_or(&[][..], |lib| lib.exports);
    let dso_handle_at = laid.got_at.checked_sub(8);
    let direct = |import: u32| -> Option<u64> {
        if let Some(target) = native.and_then(|m| {
            m.direct
                .binary_search_by_key(&import, |&(row, _)| row)
                .ok()
                .and_then(|i| m.direct.get(i).map(|&(_, at)| at))
        }) {
            return Some(target);
        }
        let idx = usize::try_from(import).ok()?;
        if laid.imports.get(idx).is_some_and(|imp| split_weak(imp.name).0 == DSO_HANDLE) {
            return dso_handle_at;
        }
        None
    };
    let mut needs_dso_handle = false;
    let mut needs_gcc_s = false;
    let mut needs_libm = false;
    let mut kept: AVec<'_, u32> = Vec::with_capacity_in(laid.imports.len(), alloc);
    let mut row_of: AVec<'_, u32> = Vec::with_capacity_in(laid.imports.len(), alloc);
    for k in 0..laid.imports.len() {
        row_of.push(u32_of(kept.len(), "an ELF import index")?);
        let idx = u32_of(k, "a program import index")?;
        if direct(idx).is_none() {
            kept.push(idx);
            if let Some(imp) = laid.imports.get(k) {
                let (sym_name, _) = split_weak(imp.name);
                if sym_name.starts_with("_Unwind_") {
                    needs_gcc_s = true;
                } else if is_libm_symbol(sym_name) {
                    needs_libm = true;
                }
            }
        } else if laid.imports.get(k).is_some_and(|imp| split_weak(imp.name).0 == DSO_HANDLE) {
            needs_dso_handle = true;
        }
    }

    let has_data = !laid.data.is_empty() || laid.zero_len > 0;
    let konst_len = u64_of(laid.konst.len(), "the const region")?;
    let data_len = u64_of(laid.data.len(), "the data region")?;
    let const_mem = up(konst_len, PAGE);
    let mut file_end = laid.const_at + const_mem;
    if !laid.data.is_empty() {
        file_end = laid.data_at + up(data_len, PAGE);
    }
    let mut vm_end = laid.end;
    let mut native_file = 0u64;
    let mut native_code_len = 0u64;
    let mut native_data_filesz = 0u64;
    if let Some(m) = native {
        native_file = up(file_end, PAGE);
        native_code_len = m.data_at - m.text_at;
        native_data_filesz = m.file_end.saturating_sub(m.data_at);
        file_end = up(native_file + native_code_len + native_data_filesz, PAGE);
        vm_end = m.end;
    }
    let meta_off = file_end;
    let meta_at = vm_end;

    // Dynamic symbols, dynamic strings, SysV hash and relocations live in the read-only meta load.
    let mut meta: AVec<'_, u8> = Vec::new_in(alloc);
    let dynsym_at = 0u64;
    meta.extend_from_slice(&[0u8; 24]);
    let mut dynstr: AVec<'_, u8> = Vec::new_in(alloc);
    dynstr.push(0);
    let soname = if let Some(lib) = library {
        let offset = u32_of(dynstr.len(), "DT_SONAME offset")?;
        dynstr.extend_from_slice(b"lib");
        dynstr.extend_from_slice(lib.name.as_bytes());
        dynstr.extend_from_slice(b".so\0");
        Some(offset)
    } else {
        None
    };
    let needed_libc = intern(&mut dynstr, LIBC.as_bytes())?;
    let mut extra_needed: AVec<'_, u32> = Vec::new_in(alloc);
    let mut has_libm = false;
    let mut has_gcc_s = false;
    if let Some(m) = native {
        for lib in m.libraries.iter() {
            if lib.as_str() == LIBM {
                has_libm = true;
            } else if lib.as_str() == LIBGCC_S {
                has_gcc_s = true;
            }
            extra_needed.push(intern(&mut dynstr, lib.as_str().as_bytes())?);
        }
    }
    if needs_libm && !has_libm {
        extra_needed.push(intern(&mut dynstr, LIBM.as_bytes())?);
    }
    if needs_gcc_s && !has_gcc_s {
        extra_needed.push(intern(&mut dynstr, LIBGCC_S.as_bytes())?);
    }
    for &idx in kept.iter() {
        let imp_idx = usize::try_from(idx).map_err(|_| ExportError::TooLarge { what: "a program import index", len: u64::from(idx) })?;
        let Some(import) = laid.imports.get(imp_idx) else {
            return Err(ExportError::TooLarge { what: "a program import index", len: u64::from(idx) });
        };
        let (sym_name, weak) = split_weak(import.name);
        let info = if weak { (STB_WEAK << 4) | STT_NOTYPE } else { GLOBAL_FUNC };
        sym(&mut meta, intern(&mut dynstr, sym_name.as_bytes())?, info, 0, 0, 0);
    }
    let native_sym_base = 1u64 + u64_of(kept.len(), "ELF symbol count")?;
    if let Some(m) = native {
        for imp in m.imports.iter() {
            let bind = if imp.weak { STB_WEAK } else { STB_GLOBAL };
            sym(&mut meta, intern(&mut dynstr, imp.name)?, (bind << 4) | STT_NOTYPE, 0, 0, 0);
        }
    }

    // Defined dynamic symbols follow every undefined import. They name `.text` (section index 1),
    // not SHN_ABS: section headers below make the definition's place explicit to ld.so and tools.
    const SHNDX_TEXT: u16 = 1;
    let export_sym_base = 1usize + kept.len() + native.map_or(0, |m| m.imports.len());
    for x in exports {
        let Some(placed) = laid.placed.iter().find(|p| p.symbol == x.symbol) else {
            return Err(ExportError::OutOfReach {
                from: x.symbol,
                target: Target { space: Space::Func, symbol: x.symbol },
            });
        };
        let name_at = intern(&mut dynstr, x.name)?;
        sym(&mut meta, name_at, (STB_GLOBAL << 4) | STT_FUNC, SHNDX_TEXT, placed.at, placed.len);
    }

    let dynstr_at = u64_of(meta.len(), "meta offset")?;
    let dynsym_len = dynstr_at;
    meta.extend_from_slice(&dynstr);
    bytes::pad(&mut meta, 8);
    let hash_at = u64_of(meta.len(), "meta offset")?;
    let total_syms = export_sym_base + exports.len();
    let nsyms = u32_of(total_syms, "DT_HASH nchain")?;
    let nbucket = u32_of(exports.len().max(1), "DT_HASH nbucket")?;
    bytes::u32(&mut meta, nbucket);
    bytes::u32(&mut meta, nsyms);
    // Fill SysV bucket and chain words directly in their on-disk slots. Undefined imports
    // have zero chain words. Only exported definitions participate in symbol lookup.
    let buckets = hash_at + 8;
    let chains = buckets + 4 * u64::from(nbucket);
    let hash_end = chains + 4 * u64::from(nsyms);
    if meta_at + hash_end > 1 << 32 {
        return Err(ExportError::TooLarge { what: "the image's 4 GiB", len: meta_at + hash_end });
    }
    meta.resize(usize_of(hash_end, "DT_HASH end")?, 0);
    for (index, export) in exports.iter().enumerate() {
        let row = u32_of(export_sym_base + index, "an exported ELF symbol index")?;
        let bucket = usize_of(buckets + 4 * u64::from(elf_hash(export.name) % nbucket), "DT_HASH bucket")?;
        let previous = bytes::get32(&meta, bucket);
        bytes::set32(&mut meta, usize_of(chains + 4 * u64::from(row), "DT_HASH chain")?, previous);
        bytes::set32(&mut meta, bucket, row);
    }
    let hash_len = hash_end - hash_at;
    bytes::pad(&mut meta, 8);
    let rela_at = u64_of(meta.len(), "meta offset")?;
    let got_end = laid.got_at + 8 * u64_of(laid.imports.len(), "program GOT length")?;
    let mut relative = 0u64;
    for f in laid.fixups.iter() {
        match f.to {
            FixTo::Rebase { target } => {
                bytes::u64(&mut meta, f.at);
                bytes::u64(&mut meta, R_AARCH64_RELATIVE);
                bytes::u64(&mut meta, target);
                relative += 1;
            }
            FixTo::Bind { import } => {
                if let Some(target) = direct(import) {
                    bytes::u64(&mut meta, f.at);
                    bytes::u64(&mut meta, R_AARCH64_RELATIVE);
                    bytes::u64(&mut meta, target);
                    relative += 1;
                }
            }
        }
    }
    if needs_dso_handle && let Some(at) = dso_handle_at {
        bytes::u64(&mut meta, at);
        bytes::u64(&mut meta, R_AARCH64_RELATIVE);
        bytes::u64(&mut meta, at);
        relative += 1;
    }
    if let Some(m) = native {
        for &(at, target) in m.relative.iter() {
            bytes::u64(&mut meta, at);
            bytes::u64(&mut meta, R_AARCH64_RELATIVE);
            bytes::u64(&mut meta, target);
            relative += 1;
        }
    }
    for f in laid.fixups.iter() {
        if let FixTo::Bind { import } = f.to
            && direct(import).is_none()
        {
            let kind = if f.at >= laid.got_at && f.at < got_end { R_AARCH64_GLOB_DAT } else { R_AARCH64_ABS64 };
            let idx = usize::try_from(import).map_err(|_| ExportError::TooLarge { what: "a program import index", len: u64::from(import) })?;
            let Some(&row) = row_of.get(idx) else {
                return Err(ExportError::TooLarge { what: "a program import index", len: u64::from(import) });
            };
            let sym = u64::from(row) + 1;
            bytes::u64(&mut meta, f.at);
            bytes::u64(&mut meta, (sym << 32) | kind);
            bytes::u64(&mut meta, 0);
        }
    }
    if let Some(m) = native {
        for r in m.other.iter() {
            bytes::u64(&mut meta, r.at);
            match r.kind {
                ElfRelaKind::Import { import, addend, got } => {
                    let kind = if got && addend == 0 { R_AARCH64_GLOB_DAT } else { R_AARCH64_ABS64 };
                    let sym = native_sym_base + u64::from(import);
                    bytes::u64(&mut meta, (sym << 32) | kind);
                    meta.extend_from_slice(&addend.to_le_bytes());
                }
                ElfRelaKind::TlsDesc { offset } => {
                    bytes::u64(&mut meta, R_AARCH64_TLSDESC);
                    bytes::u64(&mut meta, offset);
                }
                ElfRelaKind::TlsTpRel { offset } => {
                    bytes::u64(&mut meta, R_AARCH64_TLS_TPREL64);
                    bytes::u64(&mut meta, offset);
                }
            }
        }
    }

    // Rust's descriptor root is one local-exec cell in this module's TLS template. Dynamic
    // relocation (symbol 0, addend = template offset) supplies the load-time TP-relative offset;
    // hard-coding it would only be correct for a main executable.
    let rust_tls = if library.is_some() && laid.has_tls {
        let Some(desc) = laid.tls_desc_at else {
            return Err(ExportError::Remeasured { what: "the Rust TLS descriptor address", measured: 1, written: 0 });
        };
        let root_at = desc + u64::try_from(blaze_code::tls::DESC_ROOT).map_err(|_| ExportError::TooLarge { what: "the TLS descriptor root", len: u64::MAX })?;
        let root_off = native.and_then(|m| m.tls).map_or(0, |t| up(t.memsz, 8));
        bytes::u64(&mut meta, root_at);
        bytes::u64(&mut meta, R_AARCH64_TLS_TPREL64);
        bytes::u64(&mut meta, root_off);
        Some(root_at)
    } else {
        None
    };
    let rela_len = u64_of(meta.len(), "meta offset")? - rela_at;

    let tls_phdr = match (laid.has_tls, native.and_then(|m| m.tls.map(|t| (m, t)))) {
        (false, None) => None,
        (true, None) => Some(TlsPhdr {
            offset: laid.data_at,
            vaddr: laid.data_at,
            filesz: 0,
            memsz: TLS_ROOT_MEMSZ,
            align: TLS_ROOT_ALIGN,
        }),
        (has_rust_tls, Some((m, c_tls))) => {
            let align = if has_rust_tls { c_tls.align.max(TLS_ROOT_ALIGN) } else { c_tls.align };
            let memsz = if has_rust_tls { up(c_tls.memsz, 8) + TLS_ROOT_MEMSZ } else { c_tls.memsz };
            let filesz = c_tls.filesz;
            let (offset, vaddr) = if filesz == 0 {
                (native_file + native_code_len, m.data_at)
            } else if c_tls.vaddr.is_multiple_of(align) {
                (m.file_of(c_tls.vaddr, native_file), c_tls.vaddr)
            } else {
                let align_usize = usize_of(align, "PT_TLS alignment")?;
                bytes::pad(&mut meta, align_usize);
                let in_meta = u64_of(meta.len(), "meta offset")?;
                let src_at = usize_of(c_tls.vaddr - m.text_at, "native .tdata offset")?;
                let src_len = usize_of(filesz, "native .tdata length")?;
                let Some(tdata_bytes) = m.bytes.get(src_at..src_at + src_len) else {
                    return Err(ExportError::Native(MergeError::Shape { what: "native .tdata past image bytes", at: c_tls.vaddr }));
                };
                meta.extend_from_slice(tdata_bytes);
                (meta_off + in_meta, meta_at + in_meta)
            };
            Some(TlsPhdr { offset, vaddr, filesz, memsz, align })
        }
    };

    let meta_len = u64_of(meta.len(), "meta length")?;
    if meta_at + meta_len > 1 << 32 {
        return Err(ExportError::TooLarge { what: "the image's 4 GiB", len: meta_at + meta_len });
    }

    // Rust TLS initialization must precede native constructors. `mod_init_at` is a slot holding
    // the synthesized initializer; its rebase target is the function address used for DT_INIT.
    let rust_init = if rust_tls.is_some() {
        let Some(slot) = laid.mod_init_at else {
            return Err(ExportError::Remeasured { what: "the Rust TLS initializer slot", measured: 1, written: 0 });
        };
        let target = laid.fixups.iter().find_map(|fixup| match fixup.to {
            FixTo::Rebase { target } if fixup.at == slot => Some(target),
            _ => None,
        }).ok_or(ExportError::Remeasured { what: "the Rust TLS initializer relocation", measured: 1, written: 0 })?;
        Some(target)
    } else {
        None
    };

    let mut dyn_entries: AVec<'_, (u64, u64)> = Vec::with_capacity_in(18 + extra_needed.len(), alloc);
    dyn_entries.push((DT_NEEDED, u64::from(needed_libc)));
    for &at in extra_needed.iter() {
        dyn_entries.push((DT_NEEDED, u64::from(at)));
    }
    if let Some(soname) = soname {
        dyn_entries.push((DT_SONAME, u64::from(soname)));
    }
    if let Some(init) = rust_init {
        dyn_entries.push((DT_INIT, init));
    }
    if let Some(inits) = native.and_then(|m| m.inits) {
        dyn_entries.push((DT_INIT_ARRAY, inits.vaddr));
        dyn_entries.push((DT_INIT_ARRAYSZ, inits.len));
    }
    let static_tls = native.is_some_and(|m| m.static_tls) || rust_tls.is_some();
    let dt_flags = if static_tls { DF_BIND_NOW | DF_STATIC_TLS } else { DF_BIND_NOW };
    dyn_entries.extend_from_slice(&[
        (DT_HASH, meta_at + hash_at),
        (DT_STRTAB, meta_at + dynstr_at),
        (DT_SYMTAB, meta_at + dynsym_at),
        (DT_STRSZ, u64_of(dynstr.len(), "DT_STRSZ")?),
        (DT_SYMENT, 24),
        (DT_RELA, meta_at + rela_at),
        (DT_RELASZ, rela_len),
        (DT_RELAENT, 24),
        (DT_RELACOUNT, relative),
        (DT_FLAGS, dt_flags),
        (DT_FLAGS_1, if library.is_some() { DF_1_NOW } else { DF_1 }),
    ]);
    if library.is_none() {
        dyn_entries.push((DT_DEBUG, 0));
    }
    dyn_entries.push((DT_NULL, 0));
    let dyn_bytes = u64_of(dyn_entries.len(), "PT_DYNAMIC entry count")? * 16;
    let dyn_limit = if needs_dso_handle { dso_handle_at.unwrap_or(laid.got_at) } else { laid.got_at };
    let dyn_room = dyn_limit.saturating_sub(laid.const_at);
    if dyn_bytes > dyn_room {
        return Err(ExportError::HeaderRoom { what: "the ELF dynamic section", need: dyn_bytes, room: dyn_room });
    }

    // ELF header/program headers. `phnum` is exact even though TEXT_AT reserves the historical
    // maximum header room; the unused interpreter slot is simply zero bytes before the note.
    out.clear();
    out.extend_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let native_missing = if native.is_some() { 0u64 } else { 2u64 };
    let phnum = MAX_PHDRS - u64::from(library.is_some()) - u64::from(!has_data) - u64::from(tls_phdr.is_none()) - native_missing;
    let phnum16 = u16::try_from(phnum).map_err(|_| ExportError::TooLarge { what: "e_phnum", len: phnum })?;
    bytes::u16(out, 3); // ET_DYN
    bytes::u16(out, 183); // EM_AARCH64
    bytes::u32(out, 1); // EV_CURRENT
    bytes::u64(out, if library.is_some() { 0 } else { laid.entry });
    bytes::u64(out, EHDR); // e_phoff
    bytes::u64(out, 0); // e_shoff, patched after the section headers are written
    bytes::u32(out, 0); // e_flags
    bytes::u16(out, 64); // e_ehsize
    bytes::u16(out, 56); // e_phentsize
    bytes::u16(out, phnum16);
    bytes::u16(out, 64); // e_shentsize
    bytes::u16(out, if library.is_some() { 8 } else { 0 }); // e_shnum
    bytes::u16(out, if library.is_some() { 7 } else { 0 }); // e_shstrndx
    phdr(out, PT_PHDR, PF_R, EHDR, EHDR, PHDR * phnum, PHDR * phnum, 8);
    if library.is_none() {
        let len = u64_of(INTERP.len(), "PT_INTERP")? + 1;
        phdr(out, PT_INTERP, PF_R, INTERP_AT, INTERP_AT, len, len, 1);
    }
    phdr(out, PT_NOTE, PF_R, NOTE_AT, NOTE_AT, NOTE_LEN, NOTE_LEN, 4);
    phdr(out, PT_LOAD, PF_R | PF_X, 0, 0, eh.end(), eh.end(), PAGE);
    phdr(out, PT_LOAD, PF_R | PF_W, laid.const_at, laid.const_at, konst_len, const_mem, PAGE);
    if has_data {
        phdr(out, PT_LOAD, PF_R | PF_W, laid.data_at, laid.data_at, data_len, laid.end - laid.data_at, PAGE);
    }
    if let Some(m) = native {
        phdr(out, PT_LOAD, PF_R | PF_X, native_file, m.text_at, native_code_len, native_code_len, PAGE);
        phdr(out, PT_LOAD, PF_R | PF_W, native_file + native_code_len, m.data_at, native_data_filesz, m.end - m.data_at, PAGE);
    }
    phdr(out, PT_LOAD, PF_R, meta_off, meta_at, meta_len, meta_len, PAGE);
    phdr(out, PT_DYNAMIC, PF_R | PF_W, laid.const_at, laid.const_at, dyn_bytes, dyn_bytes, 8);
    if let Some(t) = tls_phdr {
        phdr(out, PT_TLS, PF_R, t.offset, t.vaddr, t.filesz, t.memsz, t.align);
    }
    phdr(out, PT_GNU_RELRO, PF_R, laid.const_at, laid.const_at, konst_len, const_mem, 1);
    let hdr_len = u64_of(eh.eh_frame_hdr.len(), "PT_GNU_EH_FRAME")?;
    phdr(out, PT_GNU_EH_FRAME, PF_R, eh.eh_frame_hdr_at, eh.eh_frame_hdr_at, hdr_len, hdr_len, 4);
    phdr(out, PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16);
    let ph_written = u64_of(out.len(), "program headers")?;
    if ph_written != EHDR + phnum * PHDR {
        return Err(ExportError::Remeasured { what: "the ELF program headers", measured: EHDR + phnum * PHDR, written: ph_written });
    }
    if library.is_none() {
        out.resize(usize_of(INTERP_AT, "INTERP_AT")?, 0);
        out.extend_from_slice(INTERP.as_bytes());
        out.push(0);
    }
    out.resize(usize_of(NOTE_AT, "NOTE_AT")?, 0);
    bytes::u32(out, 4); // namesz
    bytes::u32(out, 16); // descsz
    bytes::u32(out, NT_GNU_BUILD_ID);
    out.extend_from_slice(b"GNU\0");
    let id_at = out.len();
    out.extend_from_slice(&[0u8; 16]);

    out.resize(usize_of(laid.text_at, "text_at")?, 0);
    out.extend_from_slice(&laid.text);
    out.resize(usize_of(eh.eh_frame_at, "eh_frame_at")?, 0);
    out.extend_from_slice(&eh.eh_frame);
    out.resize(usize_of(eh.except_tab_at, "except_tab_at")?, 0);
    out.extend_from_slice(&eh.except_tab);
    out.resize(usize_of(eh.eh_frame_hdr_at, "eh_frame_hdr_at")?, 0);
    out.extend_from_slice(&eh.eh_frame_hdr);
    out.resize(usize_of(laid.const_at, "const_at")?, 0);
    out.extend_from_slice(&laid.konst);
    let const_at_usize = usize_of(laid.const_at, "const_at")?;
    for (k, &(tag, value)) in dyn_entries.iter().enumerate() {
        let at = const_at_usize + 16 * k;
        bytes::set64(out, at, tag);
        bytes::set64(out, at + 8, value);
    }
    out.resize(usize_of(laid.const_at + const_mem, "const end")?, 0);
    if !laid.data.is_empty() {
        out.resize(usize_of(laid.data_at, "data_at")?, 0);
        out.extend_from_slice(&laid.data);
    }
    if let Some(root_at) = rust_tls {
        let off = usize_of(root_at, "the Rust TLS root cell")?;
        if off + 8 > out.len() {
            return Err(ExportError::Remeasured { what: "the Rust TLS root cell", measured: u64_of(out.len(), "the file length")?, written: root_at + 8 });
        }
        bytes::set64(out, off, 0);
    }
    if let Some(m) = native {
        let native_file_usize = usize_of(native_file, "native_file")?;
        out.resize(native_file_usize, 0);
        out.extend_from_slice(m.bytes);
        let native_end = usize_of(native_file + native_code_len + native_data_filesz, "native end")?;
        if out.len() < native_end {
            out.resize(native_end, 0);
        }
        for &(at, _) in m.relative.iter() {
            let off = usize_of(m.file_of(at, native_file), "native relative slot")?;
            bytes::set64(out, off, 0);
        }
        for r in m.other.iter() {
            let off = usize_of(m.file_of(r.at, native_file), "native relocation slot")?;
            bytes::set64(out, off, 0);
            if matches!(r.kind, ElfRelaKind::TlsDesc { .. }) {
                bytes::set64(out, off + 8, 0);
            }
        }
    }
    out.resize(usize_of(meta_off, "meta_off")?, 0);
    out.extend_from_slice(&meta);

    if library.is_some() {
        // Minimal allocated section headers. Dynamic linking itself uses program headers and dynamic
        // tags, but defined `Elf64_Sym` values need a real section index; `.text` is therefore exact
        // and the tables the dynamic tags name are exposed for independent ELF inspection.
        let mut shstr: AVec<'_, u8> = Vec::new_in(alloc);
        shstr.push(0);
        let text_name = intern(&mut shstr, b".text")?;
        let dynamic_name = intern(&mut shstr, b".dynamic")?;
        let dynsym_name = intern(&mut shstr, b".dynsym")?;
        let dynstr_name = intern(&mut shstr, b".dynstr")?;
        let hash_name = intern(&mut shstr, b".hash")?;
        let rela_name = intern(&mut shstr, b".rela.dyn")?;
        let shstr_name = intern(&mut shstr, b".shstrtab")?;
        let shstr_off = u64_of(out.len(), ".shstrtab offset")?;
        out.extend_from_slice(&shstr);
        while out.len() % 8 != 0 {
            out.push(0);
        }
        let shoff = u64_of(out.len(), "section header offset")?;
        shdr(out, 0, SHT_NULL, 0, 0, 0, 0, 0, 0, 0, 0);
        shdr(out, text_name, SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, laid.text_at, laid.text_at, u64_of(laid.text.len(), "the text section")?, 0, 0, 16, 0);
        shdr(out, dynamic_name, SHT_DYNAMIC, SHF_ALLOC | SHF_WRITE, laid.const_at, laid.const_at, dyn_bytes, 4, 0, 8, 16);
        shdr(out, dynsym_name, SHT_DYNSYM, SHF_ALLOC, meta_at + dynsym_at, meta_off + dynsym_at, dynsym_len, 4, 1, 8, 24);
        shdr(out, dynstr_name, SHT_STRTAB, SHF_ALLOC, meta_at + dynstr_at, meta_off + dynstr_at, u64_of(dynstr.len(), "the dynamic string table")?, 0, 0, 1, 0);
        shdr(out, hash_name, SHT_HASH, SHF_ALLOC, meta_at + hash_at, meta_off + hash_at, hash_len, 3, 0, 4, 4);
        shdr(out, rela_name, SHT_RELA, SHF_ALLOC, meta_at + rela_at, meta_off + rela_at, rela_len, 3, 0, 8, 24);
        shdr(out, shstr_name, SHT_STRTAB, 0, 0, shstr_off, u64_of(shstr.len(), "the section string table")?, 0, 0, 1, 0);
        bytes::set64(out, 40, shoff);
    }

    let mut hasher = Hasher::new(ID_DOMAIN);
    hasher.update(seed);
    hasher.update(&out[..]);
    let digest = hasher.finish().0;
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    out[id_at..id_at + 16].copy_from_slice(&id);
    Ok(id)
}
