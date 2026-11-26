//! An independent Rust CodeRecord exported through the actual host dynamic loader.
use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_export::{CExport, Data, DataPlace, DataWord, Entry, Function, Panic, Platform, ProgramIn, Tier, TlsRuntime, export};
use blaze_own::{AVec, Alloc, Vec};

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
#[path = "load.rs"]
mod load;

const PLATFORM: Platform = if cfg!(all(target_os = "linux", target_env = "gnu", target_arch = "aarch64")) { Platform::LinuxArm64 } else { Platform::MacosArm64 };

fn record<'a>(code: &[u32], target: Option<(Space, Symbol)>, outside: Option<&str>, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for word in code {
        r.code.extend_from_slice(&word.to_le_bytes());
    }
    if let Some((space, symbol)) = target {
        let target = r.target(Target { space, symbol });
        r.sites.push(Site { at: 0, kind: SiteKind::Load, target, addend: 0 });
    }
    if let Some(name) = outside {
        let target = r.outside(name);
        r.sites.push(Site { at: 12, kind: SiteKind::Call, target, addend: 0 });
    }
    let mut bytes = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut bytes);
    bytes
}

#[test]
fn cdylib_has_no_entry_and_exports_reached_rust_records() {
    let a = blaze_own::testing::heap().alloc();
    let answer = record(&[0x52800540, 0xd65f03c0], None, None, a); // mov w0,#42; ret
    let symbol = Symbol::of_name("independent_rust_answer");
    let cell = Symbol::of_name("image_identity");
    let dso = Symbol::of_name("__dso_handle");
    let accessor = record(&[0x90000000, 0xf9400000, 0xf9400000, 0xd65f03c0], Some((Space::Static, cell)), None, a);
    let functions = [
        Function { symbol, record: CodeView::read(&answer).unwrap(), tier: Tier::Fast },
        Function { symbol: cell, record: CodeView::read(&accessor).unwrap(), tier: Tier::Fast },
    ];
    let exports = [CExport { name: b"answer", symbol }, CExport { name: b"identity", symbol: cell }];
    let data = [Data {
        target: Target { space: Space::Static, symbol: cell },
        place: DataPlace::ReadOnly,
        bytes: &[0; 8],
        align: 8,
        words: &[DataWord { at: 0, target: Some(Target { space: Space::Outside, symbol: dso }), addend: 0 }],
        outside: &[(dso, "__dso_handle")],
    }];
    let program = ProgramIn {
        name: "rust_record",
        functions: &functions,
        data: &data,
        entry: Entry::Cdylib,
        panic: Panic::Abort,
        personality: None,
        tls: None,
        profile: None,
        natives: None,
        exports: &exports,
    };
    let mut out = Vec::new_in(a);
    let report = export(&program, PLATFORM, &mut out, a).unwrap();
    assert_eq!(report.made, 0);
    assert_eq!(report.imports, 0, "the image supplies its own __dso_handle");
    if PLATFORM == Platform::MacosArm64 {
        let word = |at| super::u32_at(&out, at);
        assert_eq!(word(12), 6);
        let mut at = 32;
        let mut symbols = None;
        let mut indirect = None;
        for _ in 0..word(16) {
            let cmd = word(at);
            assert_ne!(cmd, 0x80000028, "a library has no LC_MAIN");
            assert_ne!(cmd, 0xe, "a library has no LC_LOAD_DYLINKER");
            if cmd == 0x2 {
                symbols = Some(word(at + 12));
            }
            if cmd == 0xb {
                assert_eq!((word(at + 20), word(at + 28)), (2, 0), "two defined symbols, zero undefined");
                assert_eq!(word(at + 60), 1, "the GOT still has its local cell");
                indirect = Some(word(word(at + 56) as usize));
            }
            at += word(at + 4) as usize;
        }
        assert_eq!(symbols, Some(2), "the two C exports are the entire nlist table");
        assert_eq!(indirect, Some(0x8000_0000), "the __dso_handle GOT cell is INDIRECT_SYMBOL_LOCAL");
    } else {
        assert_eq!(&out[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes(out[16..18].try_into().unwrap()), 3, "ET_DYN");
        assert_eq!(u64::from_le_bytes(out[24..32].try_into().unwrap()), 0, "e_entry");
    }
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    load::run(&out, program.name, "rust_cdylib::cdylib_has_no_entry_and_exports_reached_rust_records");
}

#[test]
fn cdylib_loader_rebases_data_binds_allocator_and_initializes_rust_tls() {
    let a = blaze_own::testing::heap().alloc();
    let datum = Symbol::of_name("library_static");
    let desc = Symbol::of_name("library_tls_desc");
    let ender = Symbol::of_name("library_tls_ender");
    let stat = record(&[0x90000000, 0xf9400000, 0xb9400000, 0xd65f03c0], Some((Space::Static, datum)), None, a);
    // The platform C library supplies malloc at load time.
    let alloc = record(&[0xa9bf7bfd, 0x910003fd, 0x52800400, 0x90000010, 0xf9400210, 0xd63f0200, 0xa8c17bfd, 0xd65f03c0], None, Some("malloc"), a);
    let ret = record(&[0xd65f03c0], None, None, a);
    let descriptor = record(&[0x90000000, 0xf9400000, 0xd65f03c0], Some((Space::Static, desc)), None, a);
    let thread_pointer = record(&[0xd53b_d040, 0xd65f_03c0], None, None, a); // mrs x0,tpidr_el0; ret
    let functions = [
        Function { symbol: datum, record: CodeView::read(&stat).unwrap(), tier: Tier::Fast },
        Function { symbol: Symbol::of_name("allocator"), record: CodeView::read(&alloc).unwrap(), tier: Tier::Fast },
        Function { symbol: ender, record: CodeView::read(&ret).unwrap(), tier: Tier::Fast },
        Function { symbol: Symbol::of_name("descriptor_accessor"), record: CodeView::read(&descriptor).unwrap(), tier: Tier::Fast },
        Function { symbol: Symbol::of_name("thread_pointer"), record: CodeView::read(&thread_pointer).unwrap(), tier: Tier::Fast },
    ];
    let data = [Data { target: Target { space: Space::Static, symbol: datum }, place: DataPlace::Writable, align: 4, bytes: &42u32.to_le_bytes(), words: &[], outside: &[] }];
    let exports = [
        CExport { name: b"allocate", symbol: functions[1].symbol },
        CExport { name: b"answer", symbol: datum },
        CExport { name: b"descriptor", symbol: functions[3].symbol },
        CExport { name: b"thread_pointer", symbol: functions[4].symbol },
    ];
    let program = ProgramIn {
        name: "rust_data_tls",
        functions: &functions,
        data: &data,
        entry: Entry::Cdylib,
        panic: Panic::Abort,
        personality: None,
        tls: Some(TlsRuntime { descriptor: desc, ender }),
        profile: None,
        natives: None,
        exports: &exports,
    };
    let mut out = Vec::new_in(a);
    let report = export(&program, PLATFORM, &mut out, a).unwrap();
    assert_eq!(report.made, 1, "only the TLS initializer, never a fake main");
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    load::run(&out, program.name, "rust_cdylib::cdylib_loader_rebases_data_binds_allocator_and_initializes_rust_tls");
}

#[test]
fn library_refuses_missing_and_duplicate_public_symbols() {
    let a = blaze_own::testing::heap().alloc();
    let symbol = Symbol::of_name("present");
    let bytes = record(&[0xd65f03c0], None, None, a);
    let functions = [Function { symbol, record: CodeView::read(&bytes).unwrap(), tier: Tier::Fast }];
    let missing = [CExport { name: b"missing", symbol: Symbol::of_name("missing") }];
    let mut program = ProgramIn {
        name: "refusals",
        functions: &functions,
        data: &[],
        entry: Entry::Cdylib,
        panic: Panic::Abort,
        personality: None,
        tls: None,
        profile: None,
        natives: None,
        exports: &missing,
    };
    let mut out = Vec::new_in(a);
    assert!(matches!(export(&program, Platform::MacosArm64, &mut out, a), Err(blaze_export::ExportError::ExportNotPlaced { .. })));
    let duplicates = [CExport { name: b"same", symbol }, CExport { name: b"same", symbol }];
    program.exports = &duplicates;
    assert!(export(&program, Platform::MacosArm64, &mut out, a).is_err());
    let bad = [CExport { name: b"bad\0name", symbol }];
    program.exports = &bad;
    assert!(export(&program, Platform::MacosArm64, &mut out, a).is_err());
}

#[test]
fn linux_library_tls_initializer_marks_loader_relocated_root_owned() {
    let a = blaze_own::testing::heap().alloc();
    let tls = TlsRuntime { descriptor: Symbol::of_name("desc"), ender: Symbol::of_name("ender") };
    let start = blaze_export::start::start(Entry::Cdylib, Some(tls), Platform::LinuxArm64, a).unwrap();
    assert!(start.entry.is_none());
    assert_eq!(start.records.len(), 1);
    let r = CodeView::read(&start.records[0].1).unwrap();
    assert_eq!(&r.code()[56..60], &0xB2410021u32.to_le_bytes());
    assert_eq!(start.tls_init, Some(start.records[0].0));
}

#[test]
fn cdylib_can_be_empty_without_a_manufactured_function() {
    let a = blaze_own::testing::heap().alloc();
    let program = ProgramIn {
        name: "empty_library",
        functions: &[],
        data: &[],
        entry: Entry::Cdylib,
        panic: Panic::Abort,
        personality: None,
        tls: None,
        profile: None,
        natives: None,
        exports: &[],
    };
    let mut out = Vec::new_in(a);
    let report = export(&program, PLATFORM, &mut out, a).unwrap();
    assert_eq!((report.functions, report.made), (0, 0));
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    load::run(&out, program.name, "rust_cdylib::cdylib_can_be_empty_without_a_manufactured_function");
}
