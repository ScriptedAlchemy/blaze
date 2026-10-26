//! Where the daemon's memory is.
//!
//! With `RA_JIT_MEMORY` naming a directory, the daemon reports what the process holds after
//! each step of warming up, and once warm and after each request it answers writes there what
//! every ingredient of the database holds, and how much of the heap is live. A build with the
//! `heap-profile` feature run with `_RJEM_MALLOC_CONF=prof:true` also writes a profile of the
//! heap next to the ingredients, for `jeprof` to read. `RA_JIT_MEMORY_EVERY` also has a thread
//! say every few seconds what jemalloc counts, without waiting for a step to end ([`ticks`]).

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use ide_db::{RootDatabase, base_db::salsa};

/// The directory `RA_JIT_MEMORY` names, when it is set.
pub(crate) fn report_dir() -> Option<PathBuf> {
    std::env::var_os("RA_JIT_MEMORY").map(PathBuf::from)
}

/// Megabytes of memory the process holds, as [`held_by`] counts it.
pub(crate) fn resident() -> Option<u64> {
    Some(held_by(std::process::id())? >> 20)
}

/// Bytes of memory a process holds: its resident set, where `/proc` says it.
#[cfg(not(target_os = "macos"))]
pub(crate) fn held_by(pid: u32) -> Option<u64> {
    kilobytes(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?, "VmRSS:")
}

/// Bytes of memory a process of this user owns: its physical footprint, which is what
/// Activity Monitor shows, and which still counts pages macOS has compressed.
#[cfg(target_os = "macos")]
pub(crate) fn held_by(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `info` has room for the `RUSAGE_INFO_V4` record the call writes.
    let ok = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
    // SAFETY: the call wrote the record when it returned 0, and zeroes are a record too.
    (ok == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint)
}

/// Bytes of memory the machine has, where the system says, or the fewer the control group of
/// the process allows: a container's, a CI job's or a systemd unit's, which the kernel holds
/// the process to however much the machine has.
#[cfg(not(target_os = "macos"))]
pub(crate) fn of_the_machine() -> Option<u64> {
    let total = kilobytes(&std::fs::read_to_string("/proc/meminfo").ok()?, "MemTotal:")?;
    let groups = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let allowed = allowed_by(&groups, |file| {
        std::fs::read_to_string(Path::new("/sys/fs/cgroup").join(file)).ok()
    });
    Some(allowed.map_or(total, |it| it.min(total)))
}

/// The fewest bytes the control groups of `/proc/self/cgroup` (`groups`) and the groups they
/// are in allow, `read` reading a file under `/sys/fs/cgroup`: `memory.max` in version 2,
/// where `max` allows any, and `memory/…/memory.limit_in_bytes` in version 1.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn allowed_by(groups: &str, read: impl Fn(&str) -> Option<String>) -> Option<u64> {
    groups
        .lines()
        .filter_map(|line| {
            let (_, line) = line.split_once(':')?;
            let (controllers, path) = line.split_once(':')?;
            let (under, file) = match controllers {
                "" => ("", "memory.max"),
                _ if controllers.split(',').any(|it| it == "memory") => {
                    ("memory/", "memory.limit_in_bytes")
                }
                _ => return None,
            };
            let mut at = path.trim_matches('/');
            let mut fewest: Option<u64> = None;
            loop {
                let dir = if at.is_empty() { String::new() } else { format!("{at}/") };
                if let Some(bytes) = read(&format!("{under}{dir}{file}"))
                    .and_then(|it| it.trim().parse::<u64>().ok())
                {
                    fewest = Some(fewest.map_or(bytes, |it| it.min(bytes)));
                }
                if at.is_empty() {
                    break fewest;
                }
                at = at.rsplit_once('/').map_or("", |(up, _)| up);
            }
        })
        .min()
}

/// Bytes of memory the machine has.
#[cfg(target_os = "macos")]
pub(crate) fn of_the_machine() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut size = size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer, and `bytes` and `size` say where it goes.
    let ok = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (ok == 0 && bytes != 0).then_some(bytes)
}

/// The bytes a line of `/proc` such as `VmRSS:   1234 kB` says.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn kilobytes(text: &str, name: &str) -> Option<u64> {
    let line = text.lines().find_map(|it| it.strip_prefix(name))?;
    line.trim().trim_end_matches("kB").trim().parse::<u64>().ok()?.checked_mul(1024)
}

/// Bytes allocated and not freed, when the allocator counts them.
pub(crate) fn live() -> Option<u64> {
    #[cfg(unix)]
    {
        tikv_jemalloc_ctl::epoch::advance().ok()?;
        tikv_jemalloc_ctl::stats::allocated::read().ok().map(|it| it as u64)
    }
    #[cfg(windows)]
    None
}

/// What jemalloc says of the pages it holds: the bytes allocated and not freed, the bytes in
/// pages it maps and touches, the bytes it maps, and the bytes of freed pages it keeps to
/// reuse rather than give back. None where the allocator does not count them.
pub(crate) fn allocator() -> Option<Allocator> {
    #[cfg(unix)]
    {
        use tikv_jemalloc_ctl::stats;
        tikv_jemalloc_ctl::epoch::advance().ok()?;
        let read = |it: Result<usize, tikv_jemalloc_ctl::Error>| it.ok().map(|it| it as u64);
        Some(Allocator {
            allocated: read(stats::allocated::read())?,
            resident: read(stats::resident::read())?,
            mapped: read(stats::mapped::read())?,
            retained: read(stats::retained::read())?,
        })
    }
    #[cfg(windows)]
    None
}

/// See [`allocator`].
#[derive(Clone, Copy)]
pub(crate) struct Allocator {
    pub(crate) allocated: u64,
    pub(crate) resident: u64,
    pub(crate) mapped: u64,
    pub(crate) retained: u64,
}

/// What an idle daemon gives back, from `RA_JIT_RECLAIM`: `0` or `off` for nothing, `1` or
/// `on` for everything, or any of `gc`, `cache` and `purge`, separated by commas, for those
/// steps alone. Everything is the default.
///
/// The collection of the types is never named without the caches cleared: `gc` clears them,
/// on every thread, before it collects (a cache entry points at a type the collection frees),
/// so `gc` alone and `gc,cache` are one step. `cache` alone clears them and collects nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reclaim {
    /// `hir::collect_ty_garbage()`: the types, arguments and clauses interned for builds
    /// that are over.
    pub(crate) types: bool,
    /// The trait solver's cache on every thread: always when `types` is.
    pub(crate) solver_caches: bool,
    /// `give_back()`: the pages of what was freed.
    pub(crate) pages: bool,
}

impl Reclaim {
    /// What `RA_JIT_RECLAIM` says: everything unless it says otherwise, and `0` or `off` for
    /// nothing. Any of `gc`, `cache` and `purge`, separated by commas, asks for those steps
    /// alone, which `harness/memgrowth.py`'s M4 runs one at a time to read what each gives back.
    pub(crate) fn of_env() -> Reclaim {
        Reclaim::of(std::env::var("RA_JIT_RECLAIM").ok().as_deref())
    }

    fn of(text: Option<&str>) -> Reclaim {
        let all = Reclaim { types: true, solver_caches: true, pages: true };
        let Some(text) = text else { return all };
        match text.trim() {
            "" | "1" | "on" | "all" => all,
            "0" | "off" | "none" => Reclaim { types: false, solver_caches: false, pages: false },
            steps => {
                let named = |it: &str| steps.split(',').any(|step| step.trim() == it);
                let types = named("gc") || named("types");
                Reclaim {
                    types,
                    solver_caches: types || named("cache") || named("caches") || named("solver"),
                    pages: named("purge") || named("pages"),
                }
            }
        }
    }

    pub(crate) fn any(self) -> bool {
        self.types || self.solver_caches || self.pages
    }
}

/// Gives the system back the pages of what was freed, which jemalloc keeps for ten seconds
/// to reuse otherwise.
pub(crate) fn give_back() {
    #[cfg(unix)]
    // SAFETY: `arena.<i>.purge` reads and writes nothing, and 4096 (`MALLCTL_ARENAS_ALL`)
    // is every arena.
    unsafe {
        use std::ptr::null_mut;
        tikv_jemalloc_sys::mallctl(
            c"arena.4096.purge".as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        );
    }
}

/// With `RA_JIT_MEMORY_EVERY` naming a number of seconds, starts a thread that says every that
/// many seconds, in the log, what the process holds and what jemalloc counts: the live bytes,
/// the pages they are in, what is mapped and what is kept for reuse. With `RA_JIT_MEMORY`
/// naming a directory it appends the same to `ticks.tsv` there, and, when the build has the
/// `heap-profile` feature and the process runs with `_RJEM_MALLOC_CONF=prof:true`, writes
/// `at-<live MB>.prof` each time the live bytes pass the next `RA_JIT_MEMORY_STEP_MB` (1024) of
/// the last one. It runs beside the workspace's loading, which the report of [`report`] can't:
/// that one waits for a step to end, and a load that is stopped for its size never gets there.
pub(crate) fn ticks() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let every = std::env::var("RA_JIT_MEMORY_EVERY").ok().and_then(|it| it.parse::<f64>().ok());
    let Some(every) = every.filter(|it| *it > 0.0) else { return };
    STARTED.call_once(|| {
        let step = std::env::var("RA_JIT_MEMORY_STEP_MB")
            .ok()
            .and_then(|it| it.parse::<u64>().ok())
            .filter(|it| *it > 0)
            .unwrap_or(1024);
        let spawned = std::thread::Builder::new().name("memory-ticks".into()).spawn(move || {
            let (dir, start) = (report_dir(), std::time::Instant::now());
            if let Some(dir) = &dir {
                _ = std::fs::create_dir_all(dir);
            }
            let mut next = 0;
            for tick in 1_u64.. {
                std::thread::sleep(std::time::Duration::from_secs_f64(every));
                let mb = |bytes: Option<u64>| bytes.map_or(0, |it| it >> 20);
                let footprint = mb(held_by(std::process::id()));
                let [live, active, resident, mapped, retained, metadata] = jemalloc_stats();
                let compiled = hir_ty_codegen::compiled_functions();
                let line = format!(
                    "{tick}\t{:.0}\t{footprint}\t{live}\t{active}\t{resident}\t{mapped}\t{retained}\t{metadata}\t{compiled}",
                    start.elapsed().as_secs_f64(),
                );
                eprintln!(
                    "memory tick {tick} at {:.0}s: {footprint} MB footprint, {live} MB live, {active} MB active, {resident} MB resident, {mapped} MB mapped, {retained} MB retained, {metadata} MB metadata, {compiled} functions compiled",
                    start.elapsed().as_secs_f64(),
                );
                let Some(dir) = &dir else { continue };
                let ticks = dir.join("ticks.tsv");
                let new = !ticks.exists();
                if let Ok(mut file) =
                    std::fs::OpenOptions::new().create(true).append(true).open(&ticks)
                {
                    use std::io::Write as _;
                    if new {
                        _ = writeln!(
                            file,
                            "tick\tseconds\tfootprint_mb\tlive_mb\tactive_mb\tresident_mb\tmapped_mb\tretained_mb\tmetadata_mb\tcompiled"
                        );
                    }
                    _ = writeln!(file, "{line}");
                }
                if live >= next {
                    let at = dir.join(format!("at-{live:07}.prof"));
                    if write_heap_profile(&at).is_ok() && at.exists() {
                        next = live + step;
                    }
                }
            }
        });
        if let Err(why) = spawned {
            eprintln!("memory: the ticks did not start: {why}");
        }
    });
}

/// Megabytes jemalloc counts as live, active, resident, mapped, kept for reuse and used for its
/// own metadata, each 0 where it can't say.
fn jemalloc_stats() -> [u64; 6] {
    #[cfg(unix)]
    {
        use tikv_jemalloc_ctl::{epoch, stats};
        if epoch::advance().is_err() {
            return [0; 6];
        }
        let mb = |bytes: Result<usize, _>| bytes.map_or(0, |it| it as u64 >> 20);
        [
            mb(stats::allocated::read()),
            mb(stats::active::read()),
            mb(stats::resident::read()),
            mb(stats::mapped::read()),
            mb(stats::retained::read()),
            mb(stats::metadata::read()),
        ]
    }
    #[cfg(windows)]
    [0; 6]
}

/// Reports what the process holds now that `step` is done.
pub(crate) fn checkpoint(step: &str) {
    if report_dir().is_none() {
        return;
    }
    let live = live().map(|it| format!(", {} MB live", it >> 20)).unwrap_or_default();
    eprintln!("memory after {step}: {} MB resident{live}", resident().unwrap_or(0));
}

/// What the daemon holds that no ingredient of the database counts: the checkouts it has
/// loaded, the files it has hashed, the symbols of rustc's executables it has read, the maps
/// a patch keeps, and the addresses of an executable that patches put back. [`set_own`] says
/// it before each report.
#[derive(Clone, Copy, Default)]
pub(crate) struct Own {
    pub(crate) checkouts: usize,
    pub(crate) files_hashed: usize,
    /// How many executables rustc linked that the daemon holds the symbols of: an entry of
    /// `RUSTC_SYMBOLS` each, not a symbol each. The row is still named `rustc symbols`.
    pub(crate) rustc_symbols: usize,
    /// The entries of every map a patch keeps of the builds that are over.
    pub(crate) rejections: usize,
    /// The addresses of the executable rustc linked that a patch put back.
    pub(crate) patch_originals: usize,
}

static OWN: std::sync::Mutex<Own> = std::sync::Mutex::new(Own {
    checkouts: 0,
    files_hashed: 0,
    rustc_symbols: 0,
    rejections: 0,
    patch_originals: 0,
});

/// Says what the daemon holds of its own, for the next [`report`] to write.
pub(crate) fn set_own(own: Own) {
    *OWN.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = own;
}

/// Writes what the daemon holds now that `step` is done to the directory `RA_JIT_MEMORY` names:
/// a line for every ingredient of the database, then what the daemon holds of its own, the
/// tables of interned types and arguments, and what the allocator says.
///
/// The trait solver's cache is cleared first, here and on every thread of the pool, as
/// rust-analyzer's `analysis_stats` clears it before it measures: the cache is per revision and
/// a build's, not a holder of the daemon's, and what it keeps of its goals would be counted as
/// held.
pub(crate) fn report(db: &mut RootDatabase, step: &str) -> anyhow::Result<()> {
    checkpoint(step);
    let Some(dir) = report_dir() else { return Ok(()) };
    crate::solving::clear_caller_and_pool();
    std::fs::create_dir_all(&dir)?;
    write_heap_profile(&dir.join(format!("{step}.prof")))?;
    std::fs::write(dir.join(format!("{step}.tsv")), table(db))?;
    Ok(())
}

/// Everything [`report`] writes, as one table: the ingredient rows, whose `count` is how many
/// there are and whose `heap` is the bytes of what they own, then `own` and `interned` rows,
/// whose `count` is a number of things, and `alloc` rows, whose `count` is bytes.
fn table(db: &mut RootDatabase) -> String {
    let mut table = ingredients(db);
    let own = *OWN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, count) in [
        ("checkouts", own.checkouts),
        ("files hashed", own.files_hashed),
        ("rustc symbols", own.rustc_symbols),
        ("rejections", own.rejections),
        ("patch originals", own.patch_originals),
    ] {
        _ = writeln!(table, "own\t{count}\t\t\t\t{name}");
    }
    for (name, count) in hir::interned_counts() {
        _ = writeln!(table, "interned\t{count}\t\t\t\t{name}");
    }
    if let Some(allocator) = allocator() {
        for (name, bytes) in [
            ("allocated", allocator.allocated),
            ("resident", allocator.resident),
            ("mapped", allocator.mapped),
            ("retained", allocator.retained),
        ] {
            _ = writeln!(table, "alloc\t{bytes}\t\t\t\t{name}");
        }
    }
    table
}

/// [`report`] once the daemon has answered a request, as `request-N`, N counting from 1.
pub(crate) fn served(db: &mut RootDatabase) -> anyhow::Result<()> {
    static SERVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if report_dir().is_none() {
        return Ok(());
    }
    let served = SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    report(db, &format!("request-{served}"))
}

/// A line for every struct and query of the database: how many there are and the bytes of
/// their fields, of the database's own records about them and, where the ingredient can
/// tell, of what the fields own on the heap.
fn ingredients(db: &mut RootDatabase) -> String {
    let db: &mut dyn salsa::Database = db;
    let usage = db.memory_usage();
    let structs = usage.structs.iter().map(|it| ("struct", it.debug_name(), it));
    let queries = usage.queries.iter().map(|(&name, it)| ("query", name, it));
    let mut rows: Vec<_> = structs.chain(queries).collect();
    rows.sort_by_key(|(_, _, it)| std::cmp::Reverse(it.size_of_fields() + it.size_of_metadata()));
    let mut table = String::from("kind\tcount\tfields\tmetadata\theap\tname\n");
    for (kind, name, it) in rows {
        let heap = it.heap_size_of_fields().map(|it| it.to_string()).unwrap_or_default();
        _ = writeln!(
            table,
            "{kind}\t{}\t{}\t{}\t{heap}\t{name}",
            it.count(),
            it.size_of_fields(),
            it.size_of_metadata(),
        );
    }
    table
}

#[cfg(feature = "heap-profile")]
fn write_heap_profile(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    // SAFETY: `opt.prof` is a `bool`.
    if !unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0") }.unwrap_or(false) {
        return Ok(());
    }
    // A profile has the addresses of the code that allocated, and on macOS nothing of where
    // the program was loaded, which `harness/heapfold.py` needs to name them.
    #[cfg(target_os = "macos")]
    {
        // SAFETY: image 0 is the program itself, which is loaded for as long as it runs.
        let header = unsafe { libc::_dyld_get_image_header(0) } as usize;
        std::fs::write(path.with_extension("load"), format!("{header:#x}\n"))?;
    }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `prof.dump` takes a C string, which `path` is for the length of the call.
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", path.as_ptr()) }
        .map_err(|e| anyhow::anyhow!("writing the heap profile: {e}"))
}

#[cfg(not(feature = "heap-profile"))]
fn write_heap_profile(_: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_of_proc_is_read_as_bytes() {
        let status = "Name:\tra-jit\nVmRSS:\t    2048 kB\nThreads:\t4\n";
        assert_eq!(kilobytes(status, "VmRSS:"), Some(2048 << 10));
        assert_eq!(kilobytes(status, "VmSwap:"), None);
        assert_eq!(kilobytes("VmRSS: many kB\n", "VmRSS:"), None);
    }

    #[test]
    fn a_control_group_allows_what_it_and_those_above_it_allow() {
        let files = |files: &'static [(&'static str, &'static str)]| {
            move |file: &str| files.iter().find(|it| it.0 == file).map(|it| it.1.to_owned())
        };
        let unit = files(&[
            ("user.slice/memory.max", "8589934592\n"),
            ("user.slice/run.scope/memory.max", "2147483648\n"),
        ]);
        assert_eq!(allowed_by("0::/user.slice/run.scope\n", &unit), Some(2 << 30));
        let above = files(&[("a/memory.max", "1073741824\n"), ("a/b/memory.max", "max\n")]);
        assert_eq!(allowed_by("0::/a/b\n", above), Some(1 << 30));
        let container = files(&[("memory.max", "4294967296\n")]);
        assert_eq!(allowed_by("0::/\n", container), Some(4 << 30));
        let v1 = files(&[("memory/docker/c1/memory.limit_in_bytes", "536870912\n")]);
        assert_eq!(
            allowed_by("5:cpu,cpuacct:/docker/c1\n4:memory:/docker/c1\n", v1),
            Some(512 << 20)
        );
        assert_eq!(
            allowed_by("0::/user.slice\n", files(&[("user.slice/memory.max", "max\n")])),
            None
        );
        assert_eq!(allowed_by("", files(&[])), None);
    }

    #[test]
    fn this_process_and_the_machine_hold_some() {
        let held = held_by(std::process::id());
        let machine = of_the_machine();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(held.is_some_and(|it| it > 0) && machine.is_some_and(|it| it > held.unwrap()));
        }
    }

    #[test]
    fn reclaim_never_collects_without_clearing() {
        let steps = ["gc", "types", "gc,purge", "gc, cache", "cache", "purge", "pages,solver", "1"];
        for text in steps {
            let reclaim = Reclaim::of(Some(text));
            assert!(!reclaim.types || reclaim.solver_caches, "{text}: {reclaim:?}");
        }
        assert!(Reclaim::of(Some("gc")).solver_caches, "gc alone clears the caches too");
        assert_eq!(
            Reclaim::of(Some("gc")),
            Reclaim { types: true, solver_caches: true, pages: false }
        );
        assert_eq!(
            Reclaim::of(Some("cache")),
            Reclaim { types: false, solver_caches: true, pages: false }
        );
        assert_eq!(
            Reclaim::of(Some("purge")),
            Reclaim { types: false, solver_caches: false, pages: true }
        );
    }

    #[test]
    fn reclaim_says_everything_by_default_and_nothing_for_off() {
        let all = Reclaim { types: true, solver_caches: true, pages: true };
        for text in [None, Some(""), Some("1"), Some("on"), Some("all")] {
            assert_eq!(Reclaim::of(text), all);
        }
        for text in ["0", "off", "none"] {
            assert!(!Reclaim::of(Some(text)).any());
        }
    }
}
