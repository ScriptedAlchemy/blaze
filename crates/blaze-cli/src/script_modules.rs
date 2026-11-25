//! A build script's compiled module, looked up by a `MacroModule` task (piece 2c; the
//! coordinator's ruling D115 (c), 2026-11-16 21:1x): ONE mechanism with the proc-macro modules
//! (macros.rs, rule 30). The kind and the record are the same: a lookup whose out is the
//! module's digest and selected producer key. A build script's subject also holds its captured
//! source and build-dependency closure, so an old module is never selected for edited inputs.
//!
//! - NAMED with the `Script` parts, in the `UnitGraph` job (work.rs), one per module the
//!   world's scripts read ([`crate::names::Names::script_modules`]); its source is the graph's
//!   source with the subject ([`part_source`]), as a proc-macro module's is: the graph's source
//!   holds the module store's listing ([`crate::scripts::MODULE_LISTING`]), so a module that
//!   comes, goes or is rebuilt asks the lookup again, and nothing else does.
//! - READ by the `Script` task (its job Needs it): its out is in the `Script`'s read list, so one
//!   rebuilt script module reruns one `Script`, and an equal module moves none (early cutoff),
//!   while the `Script`'s own source holds no listing.
//! - ABSENT: a filed failure naming the script and the command that builds the module; the
//!   `Script` fails `Waited` on it and its crate is named not checked (not_checked.rs).
//! - The run consumes the selected key and digest in this task's payload, without repeating
//!   selection. Old and new source records remain immutable and may coexist.
//!
//! Interim (D110): the store is scanned on the filesystem as the runtime scans it; one scan a
//! lookup, cached by the lookup's record.

use blaze_drive::{Kind, Name, Task, TaskFailure, Why};
use blaze_ids::ContentId;
use bstd::cx::Cx;

use crate::CliError;
use crate::work::{Did, Made, held_str};

use crate::scripts::RanAs;

// "2": the subject holds the run's placement (a native script's record is a host binary,
// `target <host>` in its identity and `bin-sha256` its digest; tempo 2026-11-17: libproc,
// reth-mdbx-sys and librocksdb-sys, placed native, were looked up as wasm modules and refused
// while their native records stood in the store).
const SUBJECT_DOMAIN: &str = "blaze-cli build-script module subject 3";
const SOURCE_DOMAIN: &str = "blaze-cli build-script module source 1";
const OUT_DOMAIN: &str = "blaze-cli build-script module out 2";

/// The rule a lookup's failure names.
pub(crate) const RULE: &str = "script_modules.rs";

/// The lookup task's name for the module of `subject`.
pub(crate) fn name(subject: ContentId) -> Name {
    Name::new(Kind::MacroModule, subject)
}

/// The lookup's part source: the graph's source (which holds the module store's listing) and
/// the subject.
pub(crate) fn part_source(graph_source: ContentId, subject: ContentId) -> ContentId {
    let mut source = [0u8; 64];
    source[..32].copy_from_slice(&graph_source.0);
    source[32..].copy_from_slice(&subject.0);
    ContentId::of(SOURCE_DOMAIN, &source)
}

/// A module lookup a crate's script reads: its subject, the world index of the crate reading
/// it, and the script unit's package, version and features (from the graph, held to the end
/// of the command with the names world). Made in names.rs `world`.
pub(crate) struct ScriptModule {
    pub subject: ContentId,
    pub crate_at: usize,
    pub ran_as: RanAs,
    pub package: &'static str,
    pub version: &'static str,
    pub features: std::vec::Vec<&'static str, std::alloc::Global>,
    pub source: blaze_proc_macro::module::Source<'static>,
    pub deps: &'static str,
}

/// What one lookup is for: the run's placement and the script unit's package, version and
/// features.
pub(crate) struct Wanted<'w> {
    pub ran_as: RanAs,
    pub package: &'w str,
    pub version: &'w str,
    pub features: &'w [&'w str],
    pub source: blaze_proc_macro::module::Source<'w>,
    pub deps: &'w str,
}

impl Wanted<'_> {
    fn identity<R>(&self, f: impl FnOnce(&blaze_proc_macro::script_module::Wanted<'_>) -> R) -> R {
        let shim = blaze_proc_macro::script_module::HexDigest::of(blaze_proc_macro::script_module::SHIM);
        let wanted = blaze_proc_macro::script_module::Wanted {
            krate: blaze_proc_macro::module::CrateId { name: self.package, version: self.version, source: self.source, deps: self.deps, features: self.features },
            toolchain: blaze_proc_macro::script_module::TOOLCHAIN.trim(),
            runner: match self.ran_as {
                RanAs::Wasm => blaze_proc_macro::script_module::Runner::Wasm { shim: shim.as_str() },
                RanAs::Native => blaze_proc_macro::script_module::Runner::Native { target: crate::graph::TRIPLE },
            },
        };
        f(&wanted)
    }

    pub(crate) fn select(&self, store: &std::path::Path) -> Result<Option<blaze_proc_macro::script_module::Selected>, blaze_proc_macro::script_unit::ModuleLookupError> {
        self.identity(|wanted| blaze_proc_macro::script_unit::module_for(store, wanted))
    }

    pub(crate) fn subject(&self, frame: &bstd::mem::Frame) -> ContentId {
        self.identity(|wanted| ContentId::of(SUBJECT_DOMAIN, bstd::mem::format_in!(frame, "{wanted}").as_bytes()))
    }
}

/// One build-script module lookup: the record `<home>/.blaze/v2/build-script/modules/<key>`
/// whose identity names `wanted`: for a wasm run a module (no `target` line), its out the
/// module's `wasm-sha256`; for a native run (D112) the host binary built for the pinned host
/// (`target <host>`), its out the binary's `bin-sha256`. The same record the `Script` run
/// then reads (blaze-proc-macro `script_unit::prepare` or `native_unit::native_outputs`).
pub(crate) fn run<'b>(cx: &Cx, home: Option<&str>, wanted: Wanted<'_>, task: &Task<'_>, held: &'b bstd::mem::Frame) -> Result<Did<'b>, CliError> {
    let _cx = cx.span("script module: look the module up");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    let Some(home) = home else {
        return Ok(Did::Hole(refused("no home: the module store is under it"), "the build-script module store is under the home, and HOME is not set"));
    };
    let store = std::path::Path::new(home).join(".blaze/v2");
    let found = wanted.select(&store);
    let asked = wanted.identity(|wanted| bstd::mem::format_in!(held, "{wanted}"));
    let Wanted { ran_as, package, version, features, .. } = wanted;
    let unread = |error: &blaze_proc_macro::script_unit::ModuleLookupError| {
        let words = bstd::mem::format_in!(held, "the build script of {package} {version}: {error}");
        Did::Hole(refused("the module store could not be read"), held_str(words.as_str(), held))
    };
    let found = match found {
        Ok(found) => found,
        Err(error) => return Ok(unread(&error)),
    };
    let Some(selected) = found else {
        let nearest = match blaze_proc_macro::script_unit::records_named(&store, package, version) {
            Ok(nearest) => nearest,
            Err(error) => return Ok(unread(&error)),
        };
        let mut beside = std::string::String::new();
        match nearest.iter().min_by_key(|(_, identity)| differing(&asked, ran_as, identity.as_str()).count()) {
            None => beside.push_str(" the store holds no record of this package and version;"),
            Some((folder, identity)) => {
                let key = folder.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
                beside.push_str(&std::format!(" the nearest of the store's {} record(s) of {package} {version}, {key}, differs in:", nearest.len()));
                for line in differing(&asked, ran_as, identity) {
                    beside.push_str(&std::format!(" `{line}`"));
                }
                beside.push(';');
            }
        }
        let how = match ran_as {
            RanAs::Wasm => "build it with `toolrun.sh proc_macro_modules` in build-script mode",
            RanAs::Native => "build it with `toolrun.sh proc_macro_modules` in build-script mode with `native` (D112)",
        };
        let words = bstd::mem::format_in!(
            held,
            "the build script of {package} {version}: no build-script module of {package} {version} with features [{}] in {}: asked for the identity lines {};{beside} {how}",
            features.join(","),
            store.join("build-script/modules").display(),
            asked.as_str(),
        );
        // Filed: the store's listing is in this part's source, so the failure stands until a
        // module comes or goes.
        return Ok(Did::FailedSaying(refused("a build-script module the store lacks"), held_str(words.as_str(), held)));
    };
    let mut made = Made::new_in(name, held);
    let proof = selected.encode();
    made.payload.extend_from_slice(&proof);
    made.out = ContentId::of(OUT_DOMAIN, &proof);
    Ok(Did::Done(made))
}

/// What of `identity` keeps it from answering `asked`: each asked line it lacks, as the record
/// has that line instead (`features ..`, `target ..`) or `(no <line>)`; and, for a wasm ask, the
/// record's own `target` line (a native host binary is not a module).
pub(crate) fn differing<'i>(asked: &'i str, ran_as: RanAs, identity: &'i str) -> impl Iterator<Item = std::string::String> + 'i {
    let lacking = asked.lines().filter(|want| !identity.lines().any(|line| line == *want)).map(move |want| {
        let field = want.split(' ').next().unwrap_or_default();
        match identity.lines().find(|line| line.split(' ').next() == Some(field)) {
            Some(has) => std::format!("{has} (asked: {want})"),
            None => std::format!("no `{want}` line"),
        }
    });
    let native_record = identity.lines().find(|line| ran_as == RanAs::Wasm && line.starts_with("target ")).map(|has| std::format!("{has} (a native host binary; asked: a wasm module)"));
    lacking.chain(native_record)
}

#[cfg(test)]
mod tests {
    use super::{RanAs, differing};

    /// tempo 2026-11-17: libproc's store record is a native host binary (`target` line). A wasm
    /// ask is refused on that line, a native ask is answered by it; nix's record lacks the
    /// `time` feature the ask names, and the refusal shows the record's own features line.
    #[test]
    fn a_lookup_names_the_line_that_differs() {
        let libproc = "blaze build-script module 3\nname libproc\nversion 0.14.11\nfeatures \ntoolchain nightly-2026-10-27\ntarget aarch64-apple-darwin\nbuilder bootstrap\n";
        let host = "aarch64-apple-darwin";
        let native = format!("name libproc\nversion 0.14.11\nfeatures \ntarget {host}");
        assert_eq!(differing(&native, RanAs::Native, libproc).count(), 0);
        let shim = format!("shim {}", blaze_proc_macro::module::sha256_hex(blaze_proc_macro::script_module::SHIM));
        let wasm = format!("name libproc\nversion 0.14.11\nfeatures \n{shim}");
        let said: std::vec::Vec<_> = differing(&wasm, RanAs::Wasm, libproc).collect();
        assert_eq!(said, [std::format!("no `{shim}` line"), "target aarch64-apple-darwin (a native host binary; asked: a wasm module)".to_owned()]);
        let nix = std::format!("blaze build-script module 3\nname nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\n{shim}\nbuilder bootstrap\n");
        let asked = format!("name nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket,time\n{shim}");
        let said: std::vec::Vec<_> = differing(&asked, RanAs::Wasm, &nix).collect();
        assert_eq!(said, ["features default,ioctl,memoffset,socket (asked: features default,ioctl,memoffset,socket,time)"]);
        // A module built with another process shim (before `found`, guest/script_process.rs)
        // is named by that line: what its rebuild changes.
        let stale = "blaze build-script module 3\nname nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\nshim c1a4bffa83fc8f619c4daf0a3c0121cc1079b345b415a6d553a910a8e8a6ede6\nbuilder bootstrap\n";
        let asked = format!("name nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\n{shim}");
        let said: std::vec::Vec<_> = differing(&asked, RanAs::Wasm, stale).collect();
        assert_eq!(said, [std::format!("shim c1a4bffa83fc8f619c4daf0a3c0121cc1079b345b415a6d553a910a8e8a6ede6 (asked: {shim})")]);
        assert_eq!(differing(&asked, RanAs::Wasm, &nix).count(), 0, "CONTROL: the record with this host's shim answers the ask");
    }
}
