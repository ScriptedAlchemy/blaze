//! The entry line and the hosted call (run.md, "Every program runs in the child"): the parent's
//! `BLAZE_LAUNCH_ENTRY` reads `hosted:<symbol>` as bstd's hosted entry and still refuses an
//! unknown form by name; and a hosted entry is called with the host's table, its `i32` the
//! status the process exits with.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr::NonNull;

use blaze_code::record::Symbol as CodeSymbol;

use super::{EntryForm, HostTableAt, PreparedEntry, RunModeError, Start, call_entry, finish_control};
use crate::native_lib::{ArgumentList, ArgsError, Loaded, NativeError, ProcessArgs, ProcessArgsOpts};

pub(crate) mod process_args_tests;

fn args(words: &[&[u8]], env: &[&[u8]]) -> ProcessArgs {
    ProcessArgs::new(ProcessArgsOpts { args: words, env, alloc: blaze_own::testing::heap().alloc() })
        .expect("the fixture's C arguments")
}

const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF";

fn symbol() -> CodeSymbol {
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index as u8 % 16) * 0x11;
    }
    CodeSymbol(bytes)
}

/// `hosted:<64 hex>` reads as [`EntryForm::Hosted`] with the symbol's bytes (either case of
/// hex), beside the two forms there were; before this piece it was refused with "a hosted entry
/// needs bstd's host table", which this assertion fails on.
#[test]
fn a_hosted_entry_line_reads_as_the_hosted_form() {
    let line = format!("hosted:{HEX}");
    assert_eq!(Start::parse(line.as_bytes()), Ok(Start { entry: symbol(), form: EntryForm::Hosted }));
    assert_eq!(Start::parse(format!("unit:{HEX}").as_bytes()).map(|it| it.form), Ok(EntryForm::Unit));
    assert_eq!(Start::parse(format!("c-main:{HEX}").as_bytes()).map(|it| it.form), Ok(EntryForm::CMain));
}

/// `comptime:<64 hex>` (D175) reads as [`EntryForm::Comptime`] with the symbol's bytes; a
/// compile-time line with a short symbol is refused as any other form's is. Before this piece it
/// was refused as a form none of the three.
#[test]
fn a_comptime_entry_line_reads_as_the_comptime_form() {
    let line = format!("comptime:{HEX}");
    assert_eq!(Start::parse(line.as_bytes()), Ok(Start { entry: symbol(), form: EntryForm::Comptime }));
    assert_eq!(Start::parse(b"comptime:abcd").map_err(|why| why.why), Err("the symbol is not 64 hex digits"));
}

/// An unknown form is refused, naming the four there are; a hosted line with a short symbol or
/// a byte that is no hex digit is refused as any other form's is.
#[test]
fn an_unknown_form_or_a_bad_symbol_is_refused_by_name() {
    let refused = |line: &str| Start::parse(line.as_bytes()).map_err(|why| why.why);
    let none = "the form is none of `unit`, `c-main`, `hosted` and `comptime`";
    assert_eq!(refused(&format!("lib:{HEX}")), Err(none));
    assert_eq!(refused(&format!("Hosted:{HEX}")), Err(none));
    assert_eq!(refused(&format!("comptime-{HEX}")), Err(none));
    assert_eq!(refused("hosted:abcd"), Err("the symbol is not 64 hex digits"));
    assert_eq!(refused(&format!("hosted:{}", "g".repeat(64))), Err("the symbol holds a character that is no hex digit"));
    let said = Start::parse(b"lib:00").expect_err("an unknown form").to_string();
    assert!(said.contains("`unit`, `c-main`, `hosted` or `comptime`"), "{said}");
}

/// A compile-time entry is called as C `main` with the host's arguments, not the process's:
/// handed two arguments it answers 2; the control, handed none, answers 0.
#[test]
fn a_comptime_entry_is_called_with_the_hosts_arguments() {
    use super::ComptimeArgs;
    extern "C" fn count(argc: c_int, argv: *const *const core::ffi::c_char) -> c_int {
        // SAFETY: `argv` holds `argc` strings and a null (the test's words below).
        if unsafe { !(*argv.add(argc as usize)).is_null() } { -1 } else { argc }
    }
    let words: [*const core::ffi::c_char; 3] = [c"macro".as_ptr(), c"--derive".as_ptr(), core::ptr::null()];
    // SAFETY: two C-string literals (static) and a null, alive across the call.
    let two = unsafe { ComptimeArgs::new(2, words.as_ptr()) };
    assert_eq!(call_entry(count as usize as u64, &PreparedEntry::Comptime(two)), 2);
    let empty: [*const core::ffi::c_char; 1] = [core::ptr::null()];
    // SAFETY: as above.
    let none = unsafe { ComptimeArgs::new(0, empty.as_ptr()) };
    assert_eq!(call_entry(count as usize as u64, &PreparedEntry::Comptime(none)), 0);
}

/// A stand-in for bstd's host table: only its address is compared, never read.
static TABLE: [u64; 4] = [0; 4];

/// A hosted entry of this form: 7 when handed [`TABLE`], 1 when handed anything else.
extern "C" fn hosted_seven(table: *const c_void) -> c_int {
    if core::ptr::eq(table, core::ptr::from_ref(&TABLE).cast()) { 7 } else { 1 }
}

/// A hosted entry is called as `extern "C" fn(*const HostTable) -> i32` with the table the
/// launcher made, and what it returns is the status `run` hands `exit` (7 here). Control: handed
/// another table, the same entry answers 1, so the 7 is the table's arriving, not a constant.
#[test]
fn a_hosted_entry_returning_seven_ends_with_seven() {
    // SAFETY: the stand-in is never read (`hosted_seven` compares its address only), and the
    // static outlives the call.
    let table = unsafe { HostTableAt::new(NonNull::from(&TABLE).cast::<c_void>()) };
    assert_eq!(call_entry(hosted_seven as usize as u64, &PreparedEntry::Hosted { table, args: args(&[], &[]) }), 7);
    static OTHER: [u64; 4] = [0; 4];
    // SAFETY: as above.
    let other = unsafe { HostTableAt::new(NonNull::from(&OTHER).cast::<c_void>()) };
    assert_eq!(call_entry(hosted_seven as usize as u64, &PreparedEntry::Hosted { table: other, args: args(&[], &[]) }), 1);
}

/// The constructor mutates both a string and the argv array, and puts that array's address
/// into its second string. Main checks that exact identity as well as both edits.
extern "C" fn edit_arguments(argc: c_int, argv: *const *const c_char, envp: *const *const c_char) {
    if argc != 3 { return; }
    // SAFETY: this fixture's three writable strings and null, with 16 bytes in the second
    // string; one writable environment string and null. No Rust reference crosses this call.
    unsafe {
        let second = argv.add(1).read().cast_mut();
        second.cast::<usize>().write_unaligned(argv as usize);
        second.add(size_of::<usize>()).write(b'X' as c_char);
        core::ptr::swap(argv.cast_mut(), argv.add(2).cast_mut());
        envp.read().cast_mut().add(6).write(b'Z' as c_char);
    }
}

extern "C" fn reads_edited_arguments(argc: c_int, argv: *const *const c_char) -> c_int {
    if argc != 3 { return 1; }
    // SAFETY: the same three fixture strings and null as edit_arguments; the second remains
    // at least size_of::<usize>() + 1 bytes even after the first word becomes pointer bytes.
    unsafe {
        let second = argv.add(1).read();
        if second.cast::<usize>().read_unaligned() != argv as usize { return 2; }
        if second.add(size_of::<usize>()).read() != b'X' as c_char { return 3; }
        if CStr::from_ptr(argv.read()).to_bytes() != b"tail" { return 4; }
        if CStr::from_ptr(argv.add(2).read()).to_bytes() != b"program" { return 5; }
        if !argv.add(3).read().is_null() { return 6; }
    }
    37
}

/// C constructors and C main share writable argv strings AND cells. Removing initialization
/// leaves main's identity check false; running it changes the result to 37. An envp edit is
/// retained as well, and all escaped pointers remain valid after PreparedEntry is gone.
#[test]
fn constructors_and_main_share_process_lived_arguments() {
    let (argv, envp) = {
        let args = args(&[b"program", b"abcdefghijklmnop", b"tail"], &[b"PROBE=value"]);
        let (argv, envp) = (args.argv(), args.envp());
        let prepared = PreparedEntry::CMain(args);
        assert_eq!(call_entry(reads_edited_arguments as usize as u64, &prepared), 2, "no constructor, no identity word");
        let inits = [edit_arguments as usize as u64];
        prepared.initialize(Loaded {
            record: blaze_launch::wire::RecordId([0x71; 32]), base: 0,
            inits_at: inits.as_ptr() as u64, inits: 1,
        }).expect("the constructor runs");
        assert_eq!(call_entry(reads_edited_arguments as usize as u64, &prepared), 37);
        (argv, envp)
    };
    assert_eq!(reads_edited_arguments(3, argv), 37, "escaped argv outlives the entry's metadata");
    // SAFETY: ProcessArgs owns these writable strings until process exit, also after main.
    unsafe {
        assert_eq!(CStr::from_ptr(envp.read()).to_bytes(), b"PROBE=Zalue");
        assert!(envp.add(1).read().is_null());
    }
}

/// The C copies borrow neither the caller's strings nor its pointer lists. Both source lists
/// can change and leave scope; an interior NUL is instead a typed refusal naming its list/row.
#[test]
fn process_arguments_copy_inputs_and_refuse_interior_nuls() {
    let args = {
        let mut word = *b"program";
        let mut env = *b"KEY=value";
        let copied = args(&[&word], &[&env, b"bare-entry", &[0xff]]);
        word.fill(b'x');
        env.fill(b'x');
        copied
    };
    // SAFETY: the complete copied lists are process-lived and terminated by null cells.
    unsafe {
        assert_eq!(CStr::from_ptr(args.argv().read()).to_bytes(), b"program");
        assert!(args.argv().add(1).read().is_null());
        assert_eq!(CStr::from_ptr(args.envp().read()).to_bytes(), b"KEY=value");
        assert_eq!(CStr::from_ptr(args.envp().add(1).read()).to_bytes(), b"bare-entry");
        assert_eq!(CStr::from_ptr(args.envp().add(2).read()).to_bytes(), &[0xff]);
        assert!(args.envp().add(3).read().is_null());
    }
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(ProcessArgs::new(ProcessArgsOpts { args: &[b"a\0b"], env: &[], alloc }).err(),
        Some(ArgsError::Nul { list: ArgumentList::Arguments, index: 0 }));
    assert_eq!(ProcessArgs::new(ProcessArgsOpts { args: &[], env: &[b"ok", b"a\0b"], alloc }).err(),
        Some(ArgsError::Nul { list: ArgumentList::Environment, index: 1 }));
}

#[derive(Clone, Copy)]
enum Cleanup { Run, OmitStop }

/// Runs the real refused-initializer cleanup against Shared's waiting control loop. The
/// watchdog only rescues the planted missing Stop (or a regression), so neither can hang CI.
fn refused_initializer_cleanup(cleanup: Cleanup) -> bool {
    use core::num::NonZeroU32;
    use core::sync::atomic::{AtomicU64, Ordering};
    use core::time::Duration;
    use blaze_core::wait::{Clock, Deadline, Monotonic, Waited};

    let waiters = crate::stub_wait::Waiters::reserve(NonZeroU32::new(1).expect("one waiter")).expect("the waiter mapping");
    let shared = crate::table::Shared::new(0, 0, 0, 0, 0..0, waiters);
    let completed = AtomicU64::new(0);
    let prepared = PreparedEntry::CMain(args(&[], &[]));
    let inits = [0u64];
    let record = blaze_launch::wire::RecordId([0x72; 32]);
    crate::driver_tests::scope(|scope| {
        let control = scope.spawn(|| {
            assert!(matches!(shared.wait(), crate::table::Event::Stop), "cleanup stops the real control wait");
        });
        let watchdog = scope.spawn(|| {
            let clock = Monotonic::new();
            let deadline = Deadline::after(clock.now(), Duration::from_secs(2));
            while completed.load(Ordering::Acquire) == 0 {
                let waited = blaze_core::wait::wait_shared(
                    "native initializer refusal: control cleanup finishes", &completed, 0, deadline, &clock,
                );
                match waited {
                    Ok(Waited::Ready(())) => {}
                    Ok(Waited::Deadline) => {
                        shared.stop();
                        return true;
                    }
                    Err(why) => {
                        shared.stop();
                        panic!("the cleanup watchdog's wait failed: {why}");
                    }
                }
            }
            false
        });
        let refused = prepared.initialize(Loaded { record, base: 0, inits_at: inits.as_ptr() as u64, inits: 1 }).map(|()| 0);
        let result = match cleanup {
            Cleanup::Run => finish_control(&shared, control, refused),
            Cleanup::OmitStop => {
                control.join().expect("the watchdog rescues the planted missing Stop");
                refused
            }
        };
        completed.store(1, Ordering::Release);
        blaze_core::wait::wake_shared(&completed);
        assert_eq!(result, Err(RunModeError::Native(NativeError::NullInit { record, index: 0 })));
        watchdog.join().expect("the cleanup watchdog ends")
    })
}

#[test]
fn a_refused_initializer_stops_and_joins_control() {
    assert!(!refused_initializer_cleanup(Cleanup::Run), "normal cleanup must not need the watchdog");
}

#[test]
fn the_cleanup_oracle_catches_an_omitted_stop() {
    assert!(refused_initializer_cleanup(Cleanup::OmitStop), "removing Stop must be observed as a stuck control loop");
}
