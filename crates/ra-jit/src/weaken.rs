//! `ra-jit weaken FILE...`: copies of rustc's Mach-O objects and archives whose definitions
//! give way to ra-jit's.
//!
//! ra-jit's object is linked ahead of what rustc built of the same crates, and defines many of
//! the same names. An ELF linker is told to keep the first definition it meets. Apple's has no
//! such flag: two definitions of a name are an error unless one is weak, and of several weak
//! ones the first on the command line stays. So every definition of the copies is made weak,
//! and private to the program as well, which keeps the loader from looking for the name in
//! the libraries it loads. Nothing else of a file changes, and a file that is no Mach-O
//! object is copied as it is.
//!
//! A whole build's line (`link::command`) tells an ELF linker no such thing, as it would
//! accept any two definitions of a name, a C library's among them. Its patch redefines
//! what the object of the build defines, and [`replaced`] makes just those definitions of the
//! object weak, the way Mach-O's are: a strong definition stays over a weak one wherever it
//! is on the line, and any other two strong ones are the linker's error as they are in
//! rustc's link.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use object::{
    LittleEndian as LE,
    elf::{FileHeader64, SHN_COMMON, SHN_UNDEF, SHT_SYMTAB, STB_GLOBAL, STB_WEAK, Sym64},
    macho::{LC_SYMTAB, MachHeader64, N_EXT, N_PEXT, N_SECT, N_STAB, N_TYPE, N_WEAK_DEF, Nlist64},
    pod,
    read::{
        archive::ArchiveFile,
        elf::{FileHeader as _, SectionHeader as _, Sym as _},
        macho::{MachHeader as _, Nlist as _},
    },
};
use rustc_hash::FxHashSet;

/// The directory of the copies, in the one the files are in.
pub(crate) const DIR: &str = "ra-jit-weak";

/// Writes the copy of each file and prints where it is, in the order given.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    for file in args {
        println!("{}", copy_of(Path::new(&file))?.display());
    }
    Ok(())
}

/// Writes the copy of `file`, unless the one there is of the file as it is, and says where
/// it is.
pub(crate) fn copy_of(file: &Path) -> anyhow::Result<PathBuf> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        anyhow::bail!("{} names no file", file.display());
    };
    let dir = dir.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(name);
    if !up_to_date(&copy, file) {
        let mut data =
            std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        weaken(&mut data);
        crate::replace(&copy, &data)?;
    }
    Ok(copy)
}

/// Whether the copy was written after the file it is of.
fn up_to_date(copy: &Path, file: &Path) -> bool {
    let written = |path: &Path| std::fs::metadata(path).and_then(|it| it.modified()).ok();
    matches!((written(copy), written(file)), (Some(copy), Some(file)) if copy > file)
}

/// Makes the definitions of an object, or of each object of an archive, weak.
fn weaken(data: &mut [u8]) {
    let members: Vec<Range<usize>> = match ArchiveFile::parse(&*data) {
        Ok(archive) => archive
            .members()
            .filter_map(|member| {
                let (start, len) = member.ok()?.file_range();
                Some(start as usize..(start + len) as usize)
            })
            .collect(),
        Err(_) => vec![0..data.len()],
    };
    for member in members {
        definitions(&mut data[member]);
    }
}

/// What becomes of a definition.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Made {
    /// It stays as it is.
    Kept,
    /// Weak, and still a name the program can export.
    Weak,
    /// Weak and private to the program.
    Private,
}

/// Makes the definitions of a Mach-O object weak; an object of another format stays as it is.
pub(crate) fn definitions(data: &mut [u8]) {
    weaken_but(data, |_, _| Made::Private);
}

/// As [`definitions`], for an object of ra-jit's. Of a program's whole, with a C `main` of
/// ra-jit's, that `main` and the functions a stand-in's allocator calls stay as they are: the
/// stand-ins rustc built have weak ones for cargo's link of them, and of two weak ones Apple's
/// linker may keep the stand-in's, which would start the program at whatever is laid out
/// after its `main`. A patch has no `main`, and gives way to the object it patches.
///
/// A name of C's (`#[no_mangle]`, `#[export_name]`) is weak and not private: a program can
/// export it (bun's `-exported_symbols_list` names its `napi_*` for the addons it loads), and
/// Apple's linker exports no private one. The names of Rust's and of ra-jit's are private.
pub(crate) fn ours(data: &mut [u8]) {
    let has_main = defined(data).iter().any(|it| it == b"_main");
    weaken_but(data, |name, _| {
        let name = name.strip_prefix(b"_").unwrap_or(name);
        let stays = name == b"main"
            || crate::stub::ALLOCATOR_FUNCTIONS.iter().any(|it| it.as_bytes() == name);
        match () {
            _ if has_main && stays => Made::Kept,
            _ if of_rust(name) => Made::Private,
            _ => Made::Weak,
        }
    });
}

/// As [`ours`], for the standard library's unit ([`crate::std_unit`]), which a program is
/// linked over. The program refers to what the unit defines and compiles its own copy of what
/// the unit has only a stand-in for, and tells the two apart by how the unit has them
/// ([`crate::defined_symbols`]). A stand-in is hidden, as in the unit of ELF, and stays hidden
/// and strong; what the unit defines for the program is made private and weak, as `ours` has
/// it. Both are private: the program exports neither, and the linker leaves out what it does
/// not reach.
pub(crate) fn library(data: &mut [u8]) {
    weaken_but(data, |name, hidden| match () {
        _ if hidden => Made::Kept,
        _ if of_rust(name.strip_prefix(b"_").unwrap_or(name)) => Made::Private,
        _ => Made::Weak,
    });
}

/// Whether a name (without Mach-O's `_`) is a symbol rustc mangled or one of ra-jit's own.
fn of_rust(name: &[u8]) -> bool {
    let v0 = name
        .strip_prefix(b"_R")
        .and_then(|it| it.first())
        .is_some_and(|it| it.is_ascii_alphanumeric());
    name.starts_with(b"_ZN") || v0 || name.starts_with(b"ra-jit.")
}

/// The names an object defines and other objects can see.
fn defined(data: &[u8]) -> Vec<Vec<u8>> {
    let mut data = data.to_vec();
    let mut names = Vec::new();
    weaken_but(&mut data, |name, _| {
        names.push(name.to_vec());
        Made::Kept
    });
    names
}

/// Gives what becomes of each definition other objects can see to `made`, with its name and
/// whether it is private to the program (hidden) already.
fn weaken_but(data: &mut [u8], mut made: impl FnMut(&[u8], bool) -> Made) {
    let Some((symbols, strings)) = symbol_table(data) else { return };
    let (Some(names), Some(_)) = (data.get(strings.clone()), data.get(symbols.clone())) else {
        return;
    };
    let names = names.to_vec();
    let Ok(symbols) = pod::slice_from_all_bytes_mut::<Nlist64<LE>>(&mut data[symbols]) else {
        return;
    };
    for symbol in symbols {
        let defined = symbol.n_type & (N_STAB | N_TYPE) == N_SECT;
        if !defined || symbol.n_type & N_EXT == 0 {
            continue;
        }
        let name = names.get(symbol.n_strx.get(LE) as usize..).unwrap_or_default();
        let name = name.split(|&it| it == 0).next().unwrap_or_default();
        let made = made(name, symbol.n_type & N_PEXT != 0);
        if made == Made::Private {
            symbol.n_type |= N_PEXT;
        }
        if made != Made::Kept {
            symbol.n_desc.set(LE, symbol.n_desc(LE) | N_WEAK_DEF);
        }
    }
}

/// Where in a 64-bit Mach-O object its symbols are, and their names.
fn symbol_table(data: &[u8]) -> Option<(Range<usize>, Range<usize>)> {
    let header = MachHeader64::<LE>::parse(data, 0).ok()?;
    let mut commands = header.load_commands(LE, data, 0).ok()?;
    while let Ok(Some(command)) = commands.next() {
        if command.cmd() != LC_SYMTAB {
            continue;
        }
        let symtab = command.symtab().ok()??;
        let start = symtab.symoff.get(LE) as usize;
        let len = symtab.nsyms.get(LE) as usize * size_of::<Nlist64<LE>>();
        let strings = symtab.stroff.get(LE) as usize;
        let strings = strings..strings + symtab.strsize.get(LE) as usize;
        return Some((start..start + len, strings));
    }
    None
}

/// A link's copy of its base object, with only the definitions the patch replaces made weak.
/// The original stays strong, including for another link with a different patch.
pub(crate) struct Replaced {
    path: PathBuf,
}

impl Replaced {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Replaced {
    fn drop(&mut self) {
        _ = std::fs::remove_file(&self.path);
    }
}

/// Makes a copy of the ELF object at `base` with the definitions of the names the ELF object
/// at `patch` defines too made weak. `None` when nothing is replaced. It is linked after the patch
/// with nothing said of duplicates ([`crate::cargo::plan::whole_link_args`]): the patch's
/// definition is the strong one and stays wherever it is on the line, and a name the patch
/// does not define stays as the object has it, so that a second definition of it from a
/// library is the linker's error. Only the bindings of those symbols change; a file that is
/// no ELF object, or a patch that defines nothing, is not touched.
///
/// Never weaken the base in place: successive patches need different definitions, a failed
/// link must not change the base, and outputs can be hard links to the same whole object.
/// The copy belongs to this link and is removed when the guard is dropped.
pub(crate) fn replaced(base: &Path, patch: &Path) -> std::io::Result<Option<Replaced>> {
    use std::io::Read as _;
    let is_elf = |path: &Path| -> std::io::Result<bool> {
        let mut head = [0; 4];
        let read = std::fs::File::open(path)?.read(&mut head)?;
        Ok(read == head.len() && head == *b"\x7fELF")
    };
    if !is_elf(patch)? || !is_elf(base)? {
        return Ok(None);
    }
    let patch = std::fs::read(patch)?;
    let names = elf_defined(&patch);
    if names.is_empty() {
        return Ok(None);
    }
    let mut original = std::fs::File::open(base)?;
    // SAFETY: the caller holds whole::read while the daemon's object is read.
    let object = unsafe { memmap2::Mmap::map(&original)? };
    let offsets = replaced_offsets(&object, &names);
    if offsets.is_empty() {
        return Ok(None);
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut copy = base.as_os_str().to_owned();
    let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    copy.push(format!(".{}.{}.replaced", std::process::id(), next));
    let path = PathBuf::from(copy);
    let mut file = std::fs::File::options().read(true).write(true).create_new(true).open(&path)?;
    let copy = Replaced { path };
    std::io::copy(&mut original, &mut file)?;
    // SAFETY: this is the link's private copy and no other code writes or maps it.
    let mut object = unsafe { memmap2::MmapMut::map_mut(&file)? };
    for at in offsets {
        object[at] = (STB_WEAK << 4) | (object[at] & 0xf);
    }
    object.flush()?;
    Ok(Some(copy))
}

/// The names a 64-bit little-endian ELF object defines with global binding: the ones another
/// object's definition conflicts with. A definition is a symbol in a section, or an absolute
/// one; undefined and common symbols are not.
fn elf_defined(data: &[u8]) -> FxHashSet<&[u8]> {
    let mut names = FxHashSet::default();
    let Some(symbols) = elf_symbols(data) else { return names };
    for symbol in symbols.table.iter().filter(|it| is_global_definition(it)) {
        names.extend(symbols.table.symbol_name(LE, symbol).ok());
    }
    names
}

/// The binding bytes of the global definitions of the ELF object `data` of `names`.
fn replaced_offsets(data: &[u8], names: &FxHashSet<&[u8]>) -> Vec<usize> {
    let Some(symbols) = elf_symbols(data) else { return Vec::new() };
    let size = size_of::<Sym64<LE>>();
    let info = std::mem::offset_of!(Sym64<LE>, st_info);
    symbols
        .table
        .iter()
        .enumerate()
        .filter(|(_, it)| is_global_definition(it))
        .filter(|(_, it)| symbols.table.symbol_name(LE, it).is_ok_and(|it| names.contains(it)))
        .map(|(index, _)| symbols.offset + index * size + info)
        .collect()
}

fn is_global_definition(symbol: &Sym64<LE>) -> bool {
    symbol.st_bind() == STB_GLOBAL && !matches!(symbol.st_shndx(LE), SHN_UNDEF | SHN_COMMON)
}

/// The symbol table of a 64-bit little-endian ELF object, and where in the file it is.
struct ElfSymbols<'data> {
    table: object::read::elf::SymbolTable<'data, FileHeader64<LE>>,
    offset: usize,
}

fn elf_symbols(data: &[u8]) -> Option<ElfSymbols<'_>> {
    let header = FileHeader64::<LE>::parse(data).ok()?;
    let sections = header.sections(LE, data).ok()?;
    let table = sections.symbols(LE, data, SHT_SYMTAB).ok()?;
    let section = sections.iter().find(|it| it.sh_type(LE) == SHT_SYMTAB)?;
    Some(ElfSymbols { table, offset: usize::try_from(section.sh_offset(LE)).ok()? })
}

#[cfg(test)]
mod tests {
    use object::write::{Mangling, Object, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
    };

    use super::*;

    const NAMES: [&str; 6] = [
        "_main",
        "_napi_create_async_work",
        "___ra_jit_global_alloc",
        "__ZN3app4work17h0123456789abcdefE",
        "__RNvCs1234_3app4work",
        "_ra-jit.0123456789abcdef.app::work",
    ];

    fn object() -> Vec<u8> {
        let mut object =
            Object::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
        object.set_mangling(Mangling::None);
        let text = object.add_section(Vec::new(), b"__text".to_vec(), SectionKind::Text);
        for (at, name) in NAMES.iter().enumerate() {
            let offset = object.append_section_data(text, &[0; 4], 4);
            assert_eq!(offset, 4 * at as u64);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: offset,
                size: 4,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        object.write().unwrap()
    }

    /// Whether each of [`NAMES`] is private and whether it is weak.
    fn made(data: &[u8]) -> Vec<(bool, bool)> {
        let (symbols, strings) = symbol_table(data).unwrap();
        let symbols = pod::slice_from_all_bytes::<Nlist64<LE>>(&data[symbols]).unwrap();
        let named = |name: &str| {
            let symbol = symbols.iter().find(|it| {
                let at = strings.start + it.n_strx.get(LE) as usize;
                data[at..].split(|&it| it == 0).next() == Some(name.as_bytes())
            });
            let symbol = symbol.unwrap();
            (symbol.n_type & N_PEXT != 0, symbol.n_desc(LE) & N_WEAK_DEF != 0)
        };
        NAMES.iter().map(|it| named(it)).collect()
    }

    /// A 64-bit ELF object that defines each of `functions` (a name and its bytes) in a section
    /// of its own, with global binding.
    fn elf(functions: &[(&str, &[u8; 4])]) -> Vec<u8> {
        let mut object = Object::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
        for (name, bytes) in functions {
            let section = object.add_section(
                Vec::new(),
                format!(".text.{name}").into_bytes(),
                SectionKind::Text,
            );
            let offset = object.append_section_data(section, *bytes, 4);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: offset,
                size: 4,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            });
        }
        object.write().unwrap()
    }

    /// The bindings of the names an ELF object defines, in the order of its symbol table.
    fn bindings(data: &[u8]) -> Vec<(String, bool)> {
        use object::{Object as _, ObjectSymbol as _};
        let file = object::File::parse(data).unwrap();
        let defined = file.symbols().filter(|it| !it.is_undefined() && !it.is_local());
        defined.map(|it| (it.name().unwrap().to_owned(), it.is_weak())).collect()
    }

    #[test]
    fn only_the_definitions_a_patch_makes_again_become_weak() {
        let base = elf(&[("entry", b"ENTR"), ("edited", b"BASE"), ("other", b"OTHR")]);
        let patch = elf(&[("edited", b"PTCH"), ("added", b"ADDD")]);
        let names = elf_defined(&patch);
        assert_eq!(names.len(), 2);
        let mut object = base.clone();
        let offsets = replaced_offsets(&object, &names);
        assert_eq!(offsets.len(), 1);
        for at in offsets {
            object[at] = (STB_WEAK << 4) | (object[at] & 0xf);
        }
        let mut weak: Vec<_> = bindings(&object).into_iter().filter(|it| it.1).collect();
        weak.sort();
        assert_eq!(weak, [("edited".to_owned(), true)]);
        // The rest of the file is as it was.
        let changed = base.iter().zip(&object).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 1);
        // Again, nothing is left to make weak.
        assert!(replaced_offsets(&object, &names).is_empty());
        // A patch that defines nothing, and one that is no ELF object, leave an object alone.
        assert!(elf_defined(&elf(&[])).is_empty());
        assert!(elf_defined(b"not an object").is_empty());
        assert!(replaced_offsets(b"not an object", &names).is_empty());
    }

    /// `ld.lld`, which links the ELF objects of a test on any host: the toolchain's, which
    /// the cargo that built the test is of, or one on the path. Nothing in the test runs rustc.
    fn lld() -> Option<std::process::Command> {
        let from_cargo = option_env!("CARGO").map(PathBuf::from).and_then(|cargo| {
            let sysroot = cargo.canonicalize().ok()?.parent()?.parent()?.to_owned();
            let lld = crate::link::lld_dir(&sysroot)?.join("ld.lld");
            Some((lld, Some(sysroot)))
        });
        let on_path = || {
            Some((PathBuf::from("ld.lld"), None))
                .filter(|_| std::process::Command::new("ld.lld").arg("--version").output().is_ok())
        };
        let (lld, sysroot) = from_cargo.or_else(on_path)?;
        let mut command = std::process::Command::new(lld);
        // The toolchain's lld on macOS finds LLVM beside it.
        if let Some(sysroot) = sysroot {
            command.env("DYLD_LIBRARY_PATH", sysroot.join("lib"));
        }
        Some(command)
    }

    /// What lld says of linking `objects` in this order into an executable that starts at
    /// `entry`, with nothing said of duplicate definitions, and the executable if it linked.
    fn linked(objects: &[&Path], out: &Path) -> Result<Vec<u8>, String> {
        let ran = lld()
            .expect("an ld.lld")
            .args(["-e", "entry"])
            .args(crate::cargo::plan::whole_link_args(false, out).unwrap())
            .args(objects)
            .arg("-o")
            .arg(out)
            .output()
            .unwrap();
        match ran.status.success() {
            true => Ok(std::fs::read(out).unwrap()),
            false => Err(String::from_utf8_lossy(&ran.stderr).into_owned()),
        }
    }

    /// The four bytes the executable has for the function `name`.
    fn code_of(executable: &[u8], name: &str) -> Vec<u8> {
        use object::{Object as _, ObjectSection as _, ObjectSymbol as _};
        let file = object::File::parse(executable).unwrap();
        let symbol = file.symbols().find(|it| it.name() == Ok(name)).unwrap();
        let section = file.section_by_index(symbol.section_index().unwrap()).unwrap();
        let at = (symbol.address() - section.address()) as usize;
        section.data().unwrap()[at..at + 4].to_vec()
    }

    #[test]
    fn a_whole_builds_link_keeps_the_patch_and_the_linkers_other_errors() {
        if lld().is_none() {
            eprintln!("skipped: no ld.lld to link with");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, data: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, data).unwrap();
            path
        };
        let base = write("base.o", &elf(&[("entry", b"ENTR"), ("edited", b"BASE")]));
        // What a build over native libraries has: two of them define one name.
        let first = write("first.o", &elf(&[("clash", b"ONE.")]));
        let second = write("second.o", &elf(&[("clash", b"TWO.")]));
        let out = dir.path().join("exe");
        let duplicate = |said: &str, name: &str| {
            assert!(said.contains(&format!("duplicate symbol: {name}")), "{said}");
        };

        // The line is the patch, the object, and the libraries, with nothing said of duplicates.
        // The object defines what the patch does: the linker refuses, until they are made weak.
        let patch = write("patch.o", &elf(&[("edited", b"PTCH")]));
        let said = linked(&[&patch, &base, &first], &out).unwrap_err();
        duplicate(&said, "edited");
        let original = std::fs::read(&base).unwrap();
        let copy = replaced(&base, &patch).unwrap().unwrap();
        let with_patch = linked(&[&patch, copy.path(), &first], &out).unwrap();
        assert_eq!(code_of(&with_patch, "edited"), b"PTCH");
        // The patch stays wherever it is on the line.
        assert_eq!(code_of(&linked(&[copy.path(), &patch], &out).unwrap(), "edited"), b"PTCH");

        // Two libraries that both define a name are an error with a patch ...
        let said = linked(&[&patch, copy.path(), &first, &second], &out).unwrap_err();
        duplicate(&said, "clash");
        assert!(!said.contains("duplicate symbol: edited"), "{said}");
        assert_eq!(std::fs::read(&base).unwrap(), original);
        let copy_path = copy.path().to_owned();
        drop(copy);
        assert!(!copy_path.exists());
        // A later patch that replaces another name cannot inherit an earlier weakening.
        let other_patch = write("other-patch.o", &elf(&[("entry", b"NEXT")]));
        let other_copy = replaced(&base, &other_patch).unwrap().unwrap();
        let collision = write("collision.o", &elf(&[("edited", b"COLL")]));
        let said = linked(&[&other_patch, other_copy.path(), &collision], &out).unwrap_err();
        duplicate(&said, "edited");
        assert_eq!(std::fs::read(&base).unwrap(), original);
        // ... and without one, a patch that defines nothing, as the first build has.
        let empty = write("empty.o", &elf(&[]));
        let base = write("whole.o", &elf(&[("entry", b"ENTR"), ("edited", b"BASE")]));
        assert!(replaced(&base, &empty).unwrap().is_none());
        let said = linked(&[&empty, &base, &first, &second], &out).unwrap_err();
        duplicate(&said, "clash");
        // With either library alone it links, so the error is theirs.
        assert!(linked(&[&empty, &base, &second], &out).is_ok());
    }

    #[test]
    fn a_name_of_cs_in_ours_can_still_be_exported() {
        let mut data = object();
        ours(&mut data);
        let (private, weak) = (true, true);
        let (kept, exported, mangled) = ((!private, !weak), (!private, weak), (private, weak));
        assert_eq!(made(&data), [kept, exported, kept, mangled, mangled, mangled]);
        let mut data = object();
        definitions(&mut data);
        assert!(made(&data).iter().all(|&it| it == (private, weak)));
    }
}
