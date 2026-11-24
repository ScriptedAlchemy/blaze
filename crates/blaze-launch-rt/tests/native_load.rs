//! The launcher's REAL load of a program's native library (NATIVE.md 3.7; run_mode.rs `run`'s
//! `native`, native_lib.rs `DlNatives`): a tiny library record written with no compiler (the
//! W4/W5 writers, blaze-export native_dylib.rs/native_so.rs, around a hand-laid image),
//! filed in a folder under its record's name, opened by `dlopen` through run mode's own path
//! (`Down::Native` read, `RunMode::load_native` over `DlNatives`), then a C name bound through
//! the library's handle the way a code record's call to it is (`native_lib::bind`) and called.
//!
//! - `a_c_function_of_the_loaded_library_returns_its_value`: C's `c_seven` (`mov w0, #7; ret`)
//!   is found through the handle and returns 7; the Rust word and pointer call the same thunk,
//!   and a GOT import binds the declared system library's `cos`.
//! - `constructors_wait_for_bindings_and_receive_the_program_arguments`: real mapped code
//!   records both bindings and argc/argv/envp, in the descriptor's initializer order.
//! - CONTROL `without_the_library_the_c_name_is_refused_by_name` (rule 23: the cause removed):
//!   the same bind with no library loaded is `RunModeError::NoNativeLibrary` naming the symbol,
//!   the typed failure a code record's call to `c_seven` meets when no `Native` was sent.
//! - `a_dlopen_refusal_names_the_unbound_symbol`: dyld's and ld.so's words for an import no
//!   library defines become `NativeError::Unresolved` naming it; other words stay `Refused`.

#![cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]

use blaze_code::record::Symbol as CodeSymbol;
#[cfg(target_os = "macos")]
use blaze_export::native_dylib::write;
#[cfg(target_os = "linux")]
use blaze_export::native_so::write;
use blaze_launch::Slot;
use blaze_launch::wire::List;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::{Declared, Down, Symbol as WireSymbol};
use blaze_launch_rt::code_unwind::Step;
use blaze_launch_rt::native_lib::{self, DlNatives, Loaded, NativeError, PATH_MOST, ProcessArgs, ProcessArgsOpts};
use blaze_launch_rt::run_mode::{Act, CName, Event, Memory, Outside, Placing, Room, RunMode, RunModeError, ThreadKey};
use blaze_native::Platform;
use blaze_native::image::{FixTo, Fixup, Image, Import, Placed, Region};
#[cfg(target_os = "linux")]
use blaze_native::image::TlsReloc;
use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::SlotId;

const LIBRARY: RecordId = RecordId([0x5c; 32]);
const IDENT: &str = "5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c";
const ENTRY: CodeSymbol = CodeSymbol([0xe0; 32]);
const RUST_WORD: WireSymbol = WireSymbol([0xa0; 32]);
const PLATFORM: Platform = if cfg!(target_os = "macos") { Platform::MacosArm64 } else { Platform::LinuxArm64 };
const START: u64 = if cfg!(target_os = "macos") { 0x4000 } else { 0x1_0000 };
const GOT: u64 = 2 * START;
const DATA: u64 = 3 * START;
const POINTER_ROW: u64 = DATA + 0x48;
const POINTER: u64 = DATA + 0x58;
const WORD: u64 = DATA + 0x80;
const INITS: u64 = DATA + 0x88;
const THREAD_DATA: u64 = DATA + 0x98;
const STATE: u64 = DATA + 0xA0;
const FIRST_INIT: u64 = START + 0x20;
const SECOND_INIT: u64 = START + 0x50;
const MATH_NAME: &[u8] = if cfg!(target_os = "macos") { b"_cos" } else { b"cos" };
const MISSING_NAME: &[u8] = if cfg!(target_os = "macos") { b"_blaze_native_load_missing" } else { b"blaze_native_load_missing" };
const WORD_NAME: &[u8] = if cfg!(target_os = "macos") { b"_rust_word" } else { b"rust_word" };
const MOV_W0_7: u32 = 0x5280_00E0;
const RET: u32 = 0xd65f_03c0;

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn thunk_of(slot: SlotId) -> u64 {
    assert_eq!(slot, SlotId(1), "this image declares exactly one Rust word");
    rust_word as *const () as u64
}

extern "C" fn rust_word() -> u64 {
    42
}

/// The run's slot table as the load sees it: the load only stubs slots and asks their thunks.
struct SlotsOnly;

impl Memory for SlotsOnly {
    fn stub(&mut self, _slot: SlotId) -> Result<(), RunModeError> {
        Ok(())
    }
    fn word(&self, slot: SlotId) -> u64 {
        0x6000_0000 + 8 * u64::from(slot.0)
    }
    fn address_word(&self, slot: SlotId) -> u64 {
        0x6800_0000 + 8 * u64::from(slot.0)
    }
    fn thunk(&self, slot: SlotId) -> u64 {
        thunk_of(slot)
    }
    fn outside(&mut self, _record: RecordId, symbol: CodeSymbol, _name: &str) -> Result<Outside, RunModeError> {
        Err(RunModeError::NoNativeLibrary { symbol })
    }
    fn reserve(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no code" })
    }
    fn reserve_hot(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no code" })
    }
    fn carry(&mut self, _step: Step, _placing: &Placing<'_>) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no code" })
    }
    fn static_word(&self, index: u32) -> u64 {
        0x5800_0000 + 8 * u64::from(index)
    }
    fn reserve_data(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no static" })
    }
    fn publish_static(&mut self, _index: u32, _at: u64, _bytes: &[u8], _room: Room) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no static" })
    }
    fn thread_key(&mut self, _ender: u64) -> Result<ThreadKey, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests make no thread-local key" })
    }
    fn write(&mut self, _at: u64, _bytes: &[u8]) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests write no thread-local row" })
    }
}

fn put32(bytes: &mut [u8], at: u64, value: u32) {
    let i = (at - START) as usize;
    bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], at: u64, value: u64) {
    let i = (at - START) as usize;
    bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
}

/// `m` resolves through the real library-name table: libm.so.6 on Linux, the SDK stub's
/// libSystem install name on macOS.
fn lib_math() -> Linked<'static> {
    match source(PLATFORM, Library { kind: LibraryKind::Dylib, name: "m" }).expect("the platform names `m`") {
        Source::Linked(linked) => linked,
        Source::Stub(at) => {
            const STUB: &[u8] = b"--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libSystem.B.dylib'\n...\n";
            at.linked(STUB).unwrap_or_else(|e| panic!("the stub of `m`: {e}"))
        }
    }
}

#[derive(Clone, Copy)]
enum Fixture {
    Bound,
    MissingImport,
    NullFirstInit,
    #[cfg(target_os = "linux")]
    ThreadLocalPointer,
}

/// ADRP to the fixture's data page, from its text page. Both platforms lay these functions
/// within one 4 KiB architectural page, even when the image's load alignment is 64 KiB.
fn data_page(register: u32) -> u32 {
    let pages = (DATA - START) >> 12;
    0x9000_0000 | ((pages as u32 & 3) << 29) | (((pages as u32 >> 2) & 0x7_FFFF) << 5) | register
}

/// The same image on both targets: a C function returning seven, two tail calls through
/// the Rust word and its pointer cell, and two deferred constructors. Constructor one
/// snapshots both bound cells and argc/argv/envp, then increments order; constructor two
/// multiplies by 16 and adds 2. Together they leave 18, reversed they leave 3, and repeating
/// either changes the result. Linux also carries ordinary initial-exec TLS=7; only the
/// TLS-pointer control moves the Rust pointer into that template.
fn image(fixture: Fixture) -> Image<'static> {
    let a = alloc();
    let pointer = match fixture {
        #[cfg(target_os = "linux")]
        Fixture::ThreadLocalPointer => THREAD_DATA,
        _ => POINTER,
    };
    let mut regions: AVec<'static, Placed> = Vec::new_in(a);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, START, 0x68, 2),
        (Region::Const, START + 0x68, 0, 0),
        (Region::EhFrame, START + 0x68, 0, 3),
        (Region::UnwindInfo, START + 0x68, 0, 2),
        (Region::Got, GOT, if cfg!(target_os = "linux") { 16 } else { 8 }, 3),
        (Region::Data, DATA, 0x80, 3),
        (Region::Words, WORD, 8, 3),
        (Region::Inits, INITS, 16, 3),
        (Region::ThreadVars, THREAD_DATA, 0, 3),
        (Region::ThreadData, THREAD_DATA, if cfg!(target_os = "linux") { 8 } else { 0 }, 3),
        (Region::ThreadZero, STATE, 0, 3),
        (Region::Zero, STATE, 48, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(Placed { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(a);
    bytes.resize((STATE - START) as usize, 0);
    put32(&mut bytes, START, MOV_W0_7);
    put32(&mut bytes, START + 4, RET);
    // adrp x16, DATA; ldr x16, [x16, #cell]; br x16. Tail calls keep the caller's LR.
    for (at, cell) in [(START + 8, WORD), (START + 0x14, POINTER)] {
        put32(&mut bytes, at, data_page(16));
        put32(&mut bytes, at + 4, 0xF940_0000 | (((cell - DATA) as u32 / 8) << 10) | (16 << 5) | 16);
        put32(&mut bytes, at + 8, 0xD61F_0200);
    }
    let first = [
        data_page(9),
        0xF940_0000 | (((WORD - DATA) as u32 / 8) << 10) | (9 << 5) | 10, // ldr x10, word
        0xF900_0000 | (((STATE + 8 - DATA) as u32 / 8) << 10) | (9 << 5) | 10,
        0xF940_0000 | (((POINTER - DATA) as u32 / 8) << 10) | (9 << 5) | 10, // ldr x10, pointer
        0xF900_0000 | (((STATE + 16 - DATA) as u32 / 8) << 10) | (9 << 5) | 10,
        0xB900_0000 | (((STATE + 24 - DATA) as u32 / 4) << 10) | (9 << 5), // str w0, argc
        0xF900_0000 | (((STATE + 32 - DATA) as u32 / 8) << 10) | (9 << 5) | 1,
        0xF900_0000 | (((STATE + 40 - DATA) as u32 / 8) << 10) | (9 << 5) | 2,
        0xB940_0000 | (((STATE - DATA) as u32 / 4) << 10) | (9 << 5) | 10,
        0x1100_054A, // add w10, w10, #1
        0xB900_0000 | (((STATE - DATA) as u32 / 4) << 10) | (9 << 5) | 10,
        RET,
    ];
    let second = [
        data_page(9),
        0xB940_0000 | (((STATE - DATA) as u32 / 4) << 10) | (9 << 5) | 10,
        0x0B0A_13EA, // add w10, wzr, w10, lsl #4
        0x1100_094A, // add w10, w10, #2
        0xB900_0000 | (((STATE - DATA) as u32 / 4) << 10) | (9 << 5) | 10,
        RET,
    ];
    for (start, code) in [(FIRST_INIT, first.as_slice()), (SECOND_INIT, second.as_slice())] {
        for (index, &instruction) in code.iter().enumerate() {
            put32(&mut bytes, start + 4 * index as u64, instruction);
        }
    }
    // BNL2 (blaze-native descriptor.rs): magic, words, inits, pointers, word_at, init_at,
    // pointer_at, name_at.
    put32(&mut bytes, DATA, native_lib::MAGIC);
    put32(&mut bytes, DATA + 4, 1);
    put32(&mut bytes, DATA + 8, 2);
    put32(&mut bytes, DATA + 12, 1);
    put64(&mut bytes, DATA + 16, WORD);
    put64(&mut bytes, DATA + 24, INITS);
    put64(&mut bytes, DATA + 32, POINTER_ROW);
    put64(&mut bytes, DATA + 40, DATA + 48);
    put32(&mut bytes, DATA + 48, 8);
    put32(&mut bytes, DATA + 52, WORD_NAME.len() as u32);
    let name_at = (DATA + 56 - START) as usize;
    bytes[name_at..name_at + WORD_NAME.len()].copy_from_slice(WORD_NAME);
    put64(&mut bytes, POINTER_ROW, pointer);
    put32(&mut bytes, POINTER_ROW + 8, 0);
    put32(&mut bytes, POINTER_ROW + 12, 0);
    put64(&mut bytes, THREAD_DATA, 7);
    let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
    fixups.push(Fixup { at: GOT, to: FixTo::Import { index: 0, addend: 0 } });
    fixups.push(Fixup { at: pointer, to: FixTo::Word { index: 0 } });
    if !matches!(fixture, Fixture::NullFirstInit) {
        fixups.push(Fixup { at: INITS, to: FixTo::rebase(FIRST_INIT) });
    }
    fixups.push(Fixup { at: INITS + 8, to: FixTo::rebase(SECOND_INIT) });
    fixups.sort_unstable_by_key(|f| f.at);
    let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
    imports.push(Import { name: if matches!(fixture, Fixture::MissingImport) { MISSING_NAME } else { MATH_NAME }, weak: false });
    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
    exports.push((if cfg!(target_os = "macos") { b"___blaze_native_v1" } else { b"__blaze_native_v1" }, DATA));
    exports.push((if cfg!(target_os = "macos") { b"_c_pointer" } else { b"c_pointer" }, START + 0x14));
    exports.push((if cfg!(target_os = "macos") { b"_c_seven" } else { b"c_seven" }, START));
    #[cfg(target_os = "linux")]
    exports.push((b"c_tls", THREAD_DATA));
    exports.push((if cfg!(target_os = "macos") { b"_c_word" } else { b"c_word" }, START + 8));
    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
    words.push(WORD_NAME);
    let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(a);
    libraries.push(lib_math());
    #[cfg(target_os = "macos")]
    let tls = Vec::new_in(a);
    #[cfg(target_os = "linux")]
    let tls = {
        let mut tls = Vec::new_in(a);
        tls.push(TlsReloc::TpRel { got_at: GOT + 8, offset: 0 });
        tls
    };
    Image {
        platform: PLATFORM,
        regions,
        bytes,
        file_start: START,
        end: STATE + 48,
        fixups,
        tls,
        imports,
        exports,
        words,
        inits: 2,
        descriptor_at: DATA,
        libraries,
        unwind: Vec::new_in(a),
        frames: Vec::new_in(a),
        eh_pointers: Vec::new_in(a),
    }
}

/// The library record filed as the parent files it: `<folder>/<the record's 64 hex>`, in a
/// folder of this test's own under the system's temporary folder.
fn filed(test: &str, fixture: Fixture) -> std::path::PathBuf {
    let mut file: AVec<'static, u8> = Vec::new_in(alloc());
    if let Err(e) = write(&image(fixture), IDENT, &mut file, alloc()) {
        panic!("the writer refused the image: {e:?}");
    }
    let folder = std::env::temp_dir().join(format!("blaze-native-load-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap_or_else(|e| panic!("{}: {e}", folder.display()));
    let mut path = [0u8; PATH_MOST];
    let len = native_lib::record_path(folder.as_os_str().as_encoded_bytes(), LIBRARY, &mut path).expect("a record path");
    let at = std::path::PathBuf::from(core::str::from_utf8(&path[..len]).expect("a UTF-8 path"));
    std::fs::write(&at, file.as_slice()).unwrap_or_else(|e| panic!("{}: {e}", at.display()));
    folder
}

fn remove_fixture(folder: &std::path::Path) {
    std::fs::remove_dir_all(folder).unwrap_or_else(|e| panic!("remove native fixture {}: {e}", folder.display()));
}

/// Run mode told the parent's `Native { LIBRARY, [RUST_WORD at slot 1] }` after the entry: the
/// act it asks the driver to carry.
fn told(mode: &mut RunMode<'static>) {
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc());
    mode.start(ENTRY, &mut acts).expect("the entry is numbered");
    let mut frame = std::vec::Vec::new();
    let pairs = [Declared { symbol: RUST_WORD, slot: Slot(1) }];
    Down::Native { library: LIBRARY, words: List::of(&pairs) }.encode(1, &mut frame).expect("Native encodes");
    acts.clear();
    mode.on(Event::Frame(&frame), &mut acts).expect("Native is read");
    assert!(acts.iter().any(|act| matches!(act, Act::Native { record } if *record == LIBRARY)), "the driver is asked to load it: {acts:?}");
}

fn load(natives: &mut DlNatives) -> Loaded {
    let mut mode = RunMode::new(alloc());
    told(&mut mode);
    match mode.load_native(natives, &mut SlotsOnly) {
        Ok(loaded) => loaded,
        Err(RunModeError::Native(NativeError::Open { record })) => panic!("{}", native_lib::open_refused(record, natives.said())),
        Err(why) => panic!("the load: {why}"),
    }
}

fn read_cell(loaded: Loaded, at: u64) -> u64 {
    // SAFETY: every caller names an 8-aligned cell in this fixture's mapped Data, Words,
    // Got or Zero region. DlNatives keeps the handle mapped for the process's lifetime.
    unsafe { ((loaded.base + at) as *const u64).read_volatile() }
}

fn process_args() -> ProcessArgs {
    ProcessArgs::new(ProcessArgsOpts { args: &[b"native-fixture", b"argument"], env: &[b"BLAZE_NATIVE_FIXTURE=present"], alloc: alloc() })
        .expect("the fixture's program arguments contain no NUL")
}

/// Real executable mappings, dynamic symbol binding and post-RELRO writes need the OS
/// loader. Both the word and the pointer call the same real Rust function; `cos` is resolved
/// through the generated library's declared system dependency and invoked from its GOT.
#[test]
fn a_c_function_of_the_loaded_library_returns_its_value() {
    let folder = filed("value", Fixture::Bound);
    let mut natives = DlNatives::new(folder.as_os_str().as_encoded_bytes()).expect("the folder fits");
    let loaded = load(&mut natives);
    assert_eq!((loaded.record, loaded.inits), (LIBRARY, 2));
    let word = read_cell(loaded, WORD);
    let slot = SlotId(1);
    assert_eq!(word, thunk_of(slot), "the Rust word holds the thunk of the slot the parent paired with it");
    assert_eq!(read_cell(loaded, POINTER), word, "C's pointer and its word have one function address");
    for name in ["c_word", "c_pointer"] {
        let address = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x77; 32]), name).expect("the C tail call binds");
        // SAFETY: these two fixture functions tail-call the bound `rust_word`, whose ABI is
        // extern C fn() -> u64. The image and function remain live throughout the call.
        let call: extern "C" fn() -> u64 = unsafe { core::mem::transmute(address as usize) };
        assert_eq!(call(), 42, "{name} calls the Rust slot's function");
    }
    let cos_at = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x78; 32]), "cos").expect("the declared math library supplies cos");
    assert_eq!(read_cell(loaded, GOT), cos_at, "the dynamic loader bound the GOT before returning");
    // SAFETY: the system library's `double cos(double)`, looked up through this image's
    // live handle, has this C ABI on both supported platforms.
    let cos: extern "C" fn(f64) -> f64 = unsafe { core::mem::transmute(read_cell(loaded, GOT) as usize) };
    assert_eq!(cos(0.0), 1.0);
    #[cfg(target_os = "linux")]
    {
        let tls_at = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x79; 32]), "c_tls").expect("ordinary TLS remains supported");
        // SAFETY: dlsym of this STT_TLS symbol returns the current thread's initialized
        // 8-byte TLS cell. The library stays loaded and this read stays on that thread.
        assert_eq!(unsafe { (tls_at as *const u64).read() }, 7);
    }
    let address = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x77; 32]), "c_seven").expect("`c_seven` binds through the handle");
    // SAFETY: `address` is `_c_seven` in the loaded image (`mov w0, #7; ret`), an
    // `extern "C" fn() -> i32` by its hand-laid code; the image stays mapped (never closed).
    let c_seven: extern "C" fn() -> i32 = unsafe { core::mem::transmute(address as usize) };
    assert_eq!(c_seven(), 7);
    remove_fixture(&folder);
    println!("native_load: c_seven=7; word and pointer both call Rust=42; declared cos=1; two inits remain deferred");
}

#[test]
fn constructors_wait_for_bindings_and_receive_the_program_arguments() {
    let folder = filed("inits", Fixture::Bound);
    let mut natives = DlNatives::new(folder.as_os_str().as_encoded_bytes()).expect("the folder fits");
    let loaded = load(&mut natives);
    assert_eq!(read_cell(loaded, STATE), 0, "dlopen must not run these constructors before the launcher binds their words");
    let args = process_args();
    native_lib::run_inits(loaded, &args).expect("both constructors run");
    assert_eq!(read_cell(loaded, STATE), 18, "the constructor list runs first then second, each once");
    assert_eq!(read_cell(loaded, STATE + 8), thunk_of(SlotId(1)), "constructor one observed the already bound word");
    assert_eq!(read_cell(loaded, STATE + 16), thunk_of(SlotId(1)), "constructor one observed the already bound pointer");
    assert_eq!(read_cell(loaded, STATE + 24), 2, "the program's argc reaches the constructor");
    assert_eq!(read_cell(loaded, STATE + 32), args.argv() as u64, "the program's actual argv reaches the constructor");
    assert_eq!(read_cell(loaded, STATE + 40), args.envp() as u64, "the program's actual environment reaches the constructor");
    remove_fixture(&folder);
    println!("native_load: constructors deferred until binding, order 1→2, program argc/argv/envp preserved");
}

#[test]
fn a_null_first_initializer_is_refused_before_any_constructor_runs() {
    let folder = filed("null-init", Fixture::NullFirstInit);
    let mut natives = DlNatives::new(folder.as_os_str().as_encoded_bytes()).expect("the folder fits");
    let loaded = load(&mut natives);
    let args = process_args();
    assert_eq!(native_lib::run_inits(loaded, &args), Err(RunModeError::Native(NativeError::NullInit { record: LIBRARY, index: 0 })));
    assert_eq!(read_cell(loaded, STATE), 0, "neither the null first constructor nor the second may run");
    remove_fixture(&folder);
    println!("native_load: a null first initializer is a typed refusal, with no constructor executed");
}

#[test]
fn a_real_missing_import_is_refused_by_name() {
    let folder = filed("missing", Fixture::MissingImport);
    let mut natives = DlNatives::new(folder.as_os_str().as_encoded_bytes()).expect("the folder fits");
    let mut mode = RunMode::new(alloc());
    told(&mut mode);
    assert_eq!(mode.load_native(&mut natives, &mut SlotsOnly), Err(RunModeError::Native(NativeError::Open { record: LIBRARY })));
    let name = core::str::from_utf8(MISSING_NAME).expect("the fixture's missing name is UTF-8");
    assert_eq!(native_lib::open_refused(LIBRARY, natives.said()), NativeError::Unresolved { record: LIBRARY, symbol: CName::new(name) });
    assert_eq!(mode.native(), None, "a refused import must not publish a loaded library");
    remove_fixture(&folder);
    println!("native_load: real dlopen refuses the missing strong import by name");
}

/// glibc allocates initial-exec TLS while dlopen is still running. The Rust pointer has no
/// ELF relocation: patching its template afterwards cannot repair already copied TLS cells.
/// The ordinary TLS=7 case above stays supported; moving the pointer into it is refused.
#[cfg(target_os = "linux")]
#[test]
fn a_rust_pointer_in_the_tls_template_is_refused_before_execution() {
    let folder = filed("tls-pointer", Fixture::ThreadLocalPointer);
    let mut natives = DlNatives::new(folder.as_os_str().as_encoded_bytes()).expect("the folder fits");
    let mut mode = RunMode::new(alloc());
    told(&mut mode);
    assert_eq!(
        mode.load_native(&mut natives, &mut SlotsOnly),
        Err(RunModeError::Native(NativeError::ThreadLocalPointer { record: LIBRARY, row: 0, at: THREAD_DATA }))
    );
    assert_eq!(mode.native(), None, "the unsupported pointer must not publish a loaded library or reach constructors");
    // The refused handle is still mapped. Reproduce the defect without calling its zero
    // function pointer: a late write changes the template, while this thread's TLS copy
    // stays zero. The production refusal above prevents exactly this split state.
    let descriptor = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x80; 32]), "__blaze_native_v1").expect("the refused image still has its descriptor");
    let tls_at = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x81; 32]), "c_tls").expect("the OS allocated this initial-exec TLS cell");
    let template = descriptor + (THREAD_DATA - DATA);
    assert_ne!(template, tls_at, "the OS's thread-local instance is separate from its image template");
    // SAFETY: all three addresses name aligned 8-byte cells of this still-mapped fixture:
    // its writable template, its zero-filled constructor state, and the calling thread's
    // TLS cell returned by dlsym. No constructor or function pointer is executed here.
    unsafe {
        assert_eq!((template as *const u64).read_volatile(), 0);
        assert_eq!((tls_at as *const u64).read(), 0);
        assert_eq!(((descriptor + STATE - DATA) as *const u64).read_volatile(), 0, "the refusal precedes every constructor");
        (template as *mut u64).write_volatile(thunk_of(SlotId(1)));
        assert_eq!((template as *const u64).read_volatile(), thunk_of(SlotId(1)));
        assert_eq!((tls_at as *const u64).read(), 0, "patching the template cannot repair the copy already made by dlopen");
    }
    remove_fixture(&folder);
    println!("native_load: Rust-pointer TLS refused before constructors; a late template patch demonstrably misses the OS's copy");
}

/// The control: no library loaded (no `Native` sent), the same name is refused by type, naming
/// the code record's symbol. Prints "native_load: control: no library, `c_seven` is NoNativeLibrary".
#[test]
fn without_the_library_the_c_name_is_refused_by_name() {
    let symbol = CodeSymbol([0x77; 32]);
    let refused = native_lib::bind::<DlNatives>(None, LIBRARY, symbol, "c_seven");
    assert_eq!(refused, Err(RunModeError::NoNativeLibrary { symbol }));
    println!("native_load: control: no library, `c_seven` is NoNativeLibrary");
}

/// Prints "native_load: dlopen's words typed: Unresolved `_missing_fn` (dyld, both forms, and
/// ld.so); other words Refused".
#[test]
fn a_dlopen_refusal_names_the_unbound_symbol() {
    let unresolved = |said: &str| native_lib::open_refused(LIBRARY, said);
    let want = NativeError::Unresolved { record: LIBRARY, symbol: CName::new("_missing_fn") };
    assert_eq!(unresolved("dlopen(/h/.blaze/v2/run/records/5c5c, 0x0006): symbol not found in flat namespace '_missing_fn'"), want);
    assert_eq!(unresolved("dlopen(/h/r/5c5c, 0x0006): Symbol not found: _missing_fn\n  Referenced from: /h/r/5c5c"), want);
    let linux = NativeError::Unresolved { record: LIBRARY, symbol: CName::new("missing_fn") };
    assert_eq!(unresolved("/h/r/5c5c: undefined symbol: missing_fn"), linux);
    assert_eq!(
        unresolved("dlopen(/h/r/5c5c, 0x0006): tried: '/h/r/5c5c' (not a mach-o file)"),
        NativeError::Refused { record: LIBRARY, said: native_lib::Said::new("tried: '/h/r/5c5c' (not a mach-o file)") }
    );
    // dyld's real refusal is longer than a C name's 96 bytes: its cause is printed whole.
    let path = "/Users/someone/.blaze/v2/run/records/80b46309f3e6d1c2a4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b";
    let long = format!("dlopen({path}, 0x0006): tried: '{path}' (missing code signature in <454250A3-0000-0000-0000-000000000000> '{path}')");
    let shown = format!("{}", native_lib::open_refused(LIBRARY, &long));
    assert!(shown.contains("missing code signature"), "the refusal's cause is cut: {shown}");
    println!("native_load: dlopen's words typed: Unresolved `_missing_fn` (dyld, both forms, and ld.so); other words Refused");
}
