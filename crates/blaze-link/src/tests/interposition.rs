//! Executable definitions that the dynamic loader must see before a shared library's.

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
    let so = contents(disk.file("out/libc.so"));
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
