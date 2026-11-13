//! Executable definitions that the dynamic loader must see before a shared library's.

use std::sync::Arc;

use object::read::elf::Sym as _;

use super::*;

/// Definitions with a shared-library namesake are exported by a PIE, without exporting
/// unrelated Rust names. The loader's GNU-hash lookup must find the executable's body.
#[test]
fn elf_executable_interposes_shared_definitions() {
    let mut disk = Disk::default();
    let names = ["malloc", "calloc", "realloc", "free", "ordinary_c_callback"];
    let bodies: Vec<_> = (10..15).map(|n| function(n, &returns(n), Vec::new())).collect();
    let mut library = Image::new(
        name("out/libc.so"),
        Output {
            kind: OutputKind::Shared {
                name: "libc.so".into(),
                exports: names.iter().map(|it| (*it).into()).collect(),
            },
            interpreter: None,
            ..output(false)
        },
    )
    .unwrap();
    disk.link(
        &mut library,
        Change {
            added: bodies.iter().collect(),
            names: names
                .iter()
                .zip(&bodies)
                .map(|(n, f)| ((*n).into(), Target::Direct(f.key)))
                .collect(),
            ..Change::default()
        },
    )
    .unwrap();
    let so: Arc<[u8]> = contents(disk.file("out/libc.so")).into();
    let start = function(1, &returns(0), Vec::new());
    let private = function(2, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut change = Change {
        added: vec![&start, &private],
        names: vec![
            ("_start".into(), Target::Direct(start.key)),
            ("unrelated".into(), Target::Direct(private.key)),
        ],
        natives: vec![Native {
            id: id(60),
            name: "libc.so",
            bytes: &so,
            kind: NativeKind::Shared { as_needed: false },
            order: 0,
        }],
        ..Change::default()
    };
    change.added.extend(&bodies);
    change
        .names
        .extend(names.iter().zip(&bodies).map(|(n, f)| ((*n).into(), Target::Direct(f.key))));
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let file = linked.elf();
    let endian = file.endian();
    let sections = file.elf_section_table();
    let syms = sections.symbols(endian, linked.bytes(), elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, linked.bytes()).unwrap().unwrap();
    let (hash, _) = sections.gnu_hash(endian, linked.bytes()).unwrap().unwrap();
    for name in names {
        let (_, symbol) = hash
            .find(endian, name.as_bytes(), elf::gnu_hash(name.as_bytes()), None, &syms, &versions)
            .unwrap_or_else(|| panic!("{name} must be dynamically exported by the executable"));
        assert!(!symbol.is_undefined(endian));
        assert_eq!(symbol.st_value(endian), linked.dynamic_symbol(name));
    }
    assert!(
        hash.find(endian, b"unrelated", elf::gnu_hash(b"unrelated"), None, &syms, &versions)
            .is_none()
    );
}

/// An ELF emitted through Image::stage exposes its own allocator to RTLD_DEFAULT.
/// Removing only its public name keeps the same reachable body but makes the loader
/// choose libc's definition, so the negative control exits 1 and the corrected image 0.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[test]
fn elf_loader_resolves_the_executables_allocator() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut native_output =
        crate::target::output("x86_64-unknown-linux-gnu", None, Debug::None, false).unwrap();
    let located =
        crate::target::locate(&mut native_output, &[], &[], std::path::Path::new("/")).unwrap();
    let libc_file = located
        .iter()
        .find(|it| it.path.file_name().is_some_and(|name| name == "libc.so.6"))
        .expect("the Linux x86-64 loader proof needs its native libc.so.6");
    let libc: Arc<[u8]> = std::fs::read(&libc_file.path).unwrap().into();
    // `_start` aligns the stack, calls dlsym(NULL, "malloc"), compares its result
    // with the address of our body and exits via SYS_exit (0 if equal, 1 otherwise).
    // The trailing literal is part of this function; no startup or external compiler
    // supplies code. The three displacement fields are at offsets 9, 14 and 21.
    let start = function(
        1,
        &[
            0x48, 0x83, 0xe4, 0xf0, // and rsp, -16
            0x31, 0xff, // xor edi, edi
            0x48, 0x8d, 0x35, 0, 0, 0, 0, // lea rsi, [rip + literal]
            0xe8, 0, 0, 0, 0, // call dlsym
            0x48, 0x8d, 0x15, 0, 0, 0, 0, // lea rdx, [rip + own malloc]
            0x48, 0x39, 0xd0, // cmp rax, rdx
            0x40, 0x0f, 0x95, 0xc7, // setne dil
            0x40, 0x0f, 0xb6, 0xff, // movzx edi, dil
            0xb8, 60, 0, 0, 0, // mov eax, SYS_exit
            0x0f, 0x05, 0x0f, 0x0b, // syscall; ud2
            b'm', b'a', b'l', b'l', b'o', b'c', 0,
        ],
        vec![
            blaze_codegen::Relocation { addend: 45 - 4, ..rel32(9, Target::Direct(key(1))) },
            rel32(14, Target::Extern("dlsym".into())),
            rel32(21, Target::Direct(key(2))),
        ],
    );
    // Forward allocator calls to libc so interposition is safe during loader setup.
    let malloc = function(
        2,
        &[0xe9, 0, 0, 0, 0],
        vec![rel32(1, Target::Extern("__libc_malloc".into()))],
    );
    let dir = std::env::temp_dir().join(format!("blaze-link-loader-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for define_name in [false, true] {
        let mut names = vec![("_start".into(), Target::Direct(start.key))];
        if define_name {
            names.push(("malloc".into(), Target::Direct(malloc.key)));
        }
        let mut disk = Disk::default();
        let mut target = output(false);
        target.interpreter = native_output.interpreter.clone();
        target.rpaths = vec![libc_file.path.parent().unwrap().to_str().unwrap().to_owned()];
        let mut image = Image::new(name(TO), target).unwrap();
        disk.link(
            &mut image,
            Change {
                added: vec![&start, &malloc],
                names,
                natives: vec![Native {
                    id: id(60),
                    name: "libc.so.6",
                    bytes: &libc,
                    kind: NativeKind::Shared { as_needed: true },
                    order: 0,
                }],
                ..Change::default()
            },
        )
        .unwrap();
        let linked = Linked::of(disk.file(TO));
        let exported = linked
            .elf()
            .dynamic_symbols()
            .any(|s| s.name() == Ok("malloc") && !s.is_undefined());
        assert_eq!(exported, define_name, "only the named definition is exported");
        let program = dir.join(if define_name { "exported" } else { "unnamed-control" });
        std::fs::write(&program, contents(disk.file(TO))).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = std::process::Command::new("timeout")
            .args(["--signal=KILL", "10s"])
            .arg(&program)
            .status()
            .expect("GNU timeout starts the bounded native ELF loader proof");
        assert_eq!(
            status.code(),
            Some(i32::from(!define_name)),
            "{}: {status}",
            program.display()
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
