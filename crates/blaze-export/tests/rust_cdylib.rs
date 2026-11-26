//! An independent Rust CodeRecord exported through the actual host dynamic loader.
use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_export::{CExport, Data, DataPlace, Entry, Function, Panic, Platform, ProgramIn, Tier, TlsRuntime, export};
use blaze_own::{AVec, Alloc, Vec};

fn record(code: &[u32], target: Option<(Space, Symbol)>, outside: Option<&str>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(Alloc::global());
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
    let mut bytes = Vec::new_in(Alloc::global());
    r.seal().unwrap().encode(&mut bytes);
    bytes
}

#[test]
fn cdylib_has_no_entry_and_exports_reached_rust_records() {
    let a = Alloc::global();
    let answer = record(&[0x52800540, 0xd65f03c0], None, None); // mov w0,#42; ret
    let symbol = Symbol::of_name("independent_rust_answer");
    let functions = [Function { symbol, record: CodeView::read(&answer).unwrap(), tier: Tier::Fast }];
    let exports = [CExport { name: b"answer", symbol }];
    let program = ProgramIn {
        name: "rust_record",
        functions: &functions,
        data: &[],
        entry: Entry::Cdylib,
        panic: Panic::Abort,
        personality: None,
        tls: None,
        profile: None,
        natives: None,
        exports: &exports,
    };
    let mut out = Vec::new_in(a);
    let report = export(&program, Platform::MacosArm64, &mut out, a).unwrap();
    assert_eq!(report.made, 0);
    assert_eq!(u32::from_le_bytes(out[12..16].try_into().unwrap()), 6);
    let mut at = 32;
    for _ in 0..u32::from_le_bytes(out[16..20].try_into().unwrap()) {
        let cmd = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        assert_ne!(cmd, 0x80000028, "a library has no LC_MAIN");
        assert_ne!(cmd, 0xe, "a library has no LC_LOAD_DYLINKER");
        at += u32::from_le_bytes(out[at + 4..at + 8].try_into().unwrap()) as usize;
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    load(&out, "assert d.answer() == 42");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn load(bytes: &[u8], assertion: &str) {
    let path = std::path::PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into())).join(format!(
        "blaze-rust-cdylib-{}-{}.dylib",
        std::process::id(),
        bytes.len()
    ));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    let output = std::process::Command::new("python3")
        .args(["-c", &format!("import ctypes,sys,signal\nsignal.alarm(15)\nlibc=ctypes.CDLL(None)\nkey=ctypes.c_uint()\nassert libc.pthread_key_create(ctypes.byref(key),None)==0\nd=ctypes.CDLL(sys.argv[1])\n{assertion}\nlibc.pthread_key_delete(key)\nprint('independent Rust CodeRecord dlopen/dlsym passed')"), path.to_str().unwrap()])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    println!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn cdylib_loader_rebases_data_binds_allocator_and_initializes_rust_tls() {
    let a = Alloc::global();
    let datum = Symbol::of_name("library_static");
    let desc = Symbol::of_name("library_tls_desc");
    let ender = Symbol::of_name("library_tls_ender");
    let stat = record(&[0x90000000, 0xf9400000, 0xb9400000, 0xd65f03c0], Some((Space::Static, datum)), None);
    // A real libSystem malloc binding, no host address embedded in the file.
    let alloc = record(&[0xa9bf7bfd, 0x910003fd, 0x52800400, 0x90000010, 0xf9400210, 0xd63f0200, 0xa8c17bfd, 0xd65f03c0], None, Some("malloc"));
    let ret = record(&[0xd65f03c0], None, None);
    let descriptor = record(&[0x90000000, 0xf9400000, 0xd65f03c0], Some((Space::Static, desc)), None);
    let functions = [
        Function { symbol: datum, record: CodeView::read(&stat).unwrap(), tier: Tier::Fast },
        Function { symbol: Symbol::of_name("allocator"), record: CodeView::read(&alloc).unwrap(), tier: Tier::Fast },
        Function { symbol: ender, record: CodeView::read(&ret).unwrap(), tier: Tier::Fast },
        Function { symbol: Symbol::of_name("descriptor_accessor"), record: CodeView::read(&descriptor).unwrap(), tier: Tier::Fast },
    ];
    let data = [Data { target: Target { space: Space::Static, symbol: datum }, place: DataPlace::Writable, align: 4, bytes: &42u32.to_le_bytes(), words: &[], outside: &[] }];
    let exports = [CExport { name: b"allocate", symbol: functions[1].symbol }, CExport { name: b"answer", symbol: datum }, CExport { name: b"descriptor", symbol: functions[3].symbol }];
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
    let report = export(&program, Platform::MacosArm64, &mut out, a).unwrap();
    assert_eq!(report.made, 1, "only the TLS initializer, never a fake main");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    load(
        &out,
        "assert d.answer() == 42\nd.descriptor.restype=ctypes.POINTER(ctypes.c_uint64)\ndesc=d.descriptor()\nassert desc[0] != key.value\nassert desc[1] == desc[0]*8 and desc[1] > 0\nd.allocate.restype=ctypes.c_void_p\np=d.allocate()\nassert p\nctypes.memset(p, 0x5a, 32)\nlibc=ctypes.CDLL(None)\nlibc.free.argtypes=[ctypes.c_void_p]\nlibc.free(p)",
    );
}

#[test]
fn library_refuses_missing_and_duplicate_public_symbols() {
    let a = Alloc::global();
    let symbol = Symbol::of_name("present");
    let bytes = record(&[0xd65f03c0], None, None);
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
    let a = Alloc::global();
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
    let a = Alloc::global();
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
    let report = export(&program, Platform::MacosArm64, &mut out, a).unwrap();
    assert_eq!((report.functions, report.made), (0, 0));
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    load(&out, "assert d is not None");
}
