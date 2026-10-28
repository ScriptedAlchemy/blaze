#!/usr/bin/env python3
"""scriptedits.py: a build script reruns across edits in blaze's whole build as it does under
cargo 1.99 (docs/distributed.md, P0.5).

usage: scriptedits.py [--work DIR] [--json OUT] [--keep] [--steps N] [--list] [case ...]
       scriptedits.py --selftest         (the comparison alone, without a build)

RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it. Linux, where the whole
build works (`RA_JIT_WHOLE_BUILD=1`, #255 is macOS's).

Each case is a project and a list of steps. A step edits files, sets variables, or does
nothing, and then both builds run on it: blaze's whole build (`cargo ra-jit build`, one daemon
for all the steps of the case, as an edit is the daemon's) and cargo's (`cargo build` into a
target directory of its own, the oracle, which blaze never runs). The two are the same project
in two directories, with the same edits made at the same steps. After each step blaze's and
cargo's

  build      exit status
  runs       how many times each script ran in the step: every script appends its package's
             name to a log of its own, outside the package, which nothing watches
  program    exit status, standard output and standard error of the program each built, run
             directly with the step's variables

have to be equal. A case that cargo itself does not run as its `runs` say (`expect`) is a case
of the harness that is wrong, and is reported as that, not as blaze's.

The cases are the verifier's probes of PR #5 (docs/reports/BUILDSCRIPT.md, the four rounds of
u8-verify): a script that counts its runs, files watched in the package and outside it, a
directory watched, a file that is not there, the variables a script watches (set, changed,
empty and unset, watched only after an edit, or from its second run), a script that stops
saying what it watches, a script that fails and is fixed, a `links` dependency whose metadata
changes or does not, an `OUT_DIR` with hard links, symlinks and extended attributes that a
rerun finds as it left them, a generated file the crate `include!`s, and a run the store
kept, which must not stand for one cargo would make. Then what the reports hint at: a script
that writes a file it watches or one of its package, a module of the script and a build
dependency, a link and a relative path, what a run says to rustc, RUSTFLAGS, `[env]` and the
manifest, a `build.rs` that appears, the daemon stopped between builds, the release profile, a
feature, and a script that cargo runs every build, in a package of two programs. And the six
findings of p05w-verify (PR #12): two builds at once, `option_env!` in the script and in a build
dependency, a package's files as cargo's `PathSource` lists them (a `CACHEDIR.TAG`, `include`,
git), the output of the last run deleted, an extended attribute in `OUT_DIR`, and no rustc and no
compiling cargo in blaze's build (`strace`, Linux; cargo's queries are counted and shown).

Python 3, stdlib only.
"""
import argparse, glob, json, os, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import RA_JIT, base_env

TIMEOUT = 900
# Edits are made this long after the last build: a file system's clock is as coarse as a tick,
# and cargo calls a file newer than its output only when it is.
TICK = float(os.environ.get("SCRIPTEDITS_TICK", "0.05"))

PRELUDE = r'''use std::{env, fs, io::Write, path::PathBuf};

fn note(tag: &str) {
    let mut log = fs::OpenOptions::new().create(true).append(true).open("@LOG@").unwrap();
    writeln!(log, "{tag}").unwrap();
}

fn out() -> PathBuf {
    PathBuf::from(env::var("OUT_DIR").unwrap())
}

/// A count a script keeps in its `OUT_DIR`, which cargo leaves for the next run.
fn bump() -> u32 {
    let file = out().join("count");
    let n = fs::read_to_string(&file).ok().and_then(|it| it.trim().parse().ok()).unwrap_or(0) + 1;
    fs::write(&file, n.to_string()).unwrap();
    n
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| "missing".to_owned())
}

fn generated(text: &str) {
    fs::write(out().join("generated.rs"), format!("pub const GENERATED: &str = {text:?};\n")).unwrap();
}
'''

MAIN = ('include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nconst EDIT: u32 = 1;\n\n'
        'fn main() {\n    println!("{} edit={}", GENERATED, EDIT);\n}\n')

MANIFEST = '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n[workspace]\n'


def main_with(edit):
    return MAIN.replace("EDIT: u32 = 1", f"EDIT: u32 = {edit}")


def single(script, files=None, outside="zero\n"):
    """The files of a case of one package, `probe`, with the script `script` after the prelude."""
    files = dict(files or {})
    files.setdefault("Cargo.toml", MANIFEST)
    files.setdefault("src/main.rs", MAIN)
    files["build.rs"] = PRELUDE + script
    if outside is not None:
        files["@OUTSIDE@"] = outside
    return files


def step(label, runs=None, write=None, remove=(), touch=(), env=None, clean=False, restart=False, args=(),
         concurrent=None, remove_output=False, xattr=None):
    """One step. `runs` is what cargo is expected to run: how many times the script of each
    package, as a dictionary, or a number for `probe`'s. `clean` removes what was built first,
    as `cargo clean` does, and has blaze's daemon stop; `restart` has the daemon stop alone, for
    the next build to load the workspace again. `args` are given to the build. `concurrent` is
    a list of variables, a build with each started at once (two clients of one daemon);
    `remove_output` removes the output each build keeps of `probe`'s last run, cargo's in its
    target directory and blaze's in its own; `xattr` is (a file in `probe`'s `OUT_DIR`, a name,
    a value): an extended attribute set on that file of each before the build."""
    if isinstance(runs, int):
        runs = {"probe": runs}
    return {"label": label, "runs": runs, "write": write or {}, "remove": list(remove),
            "touch": list(touch), "env": env or {}, "clean": clean, "restart": restart, "args": list(args),
            "concurrent": concurrent, "remove_output": remove_output, "xattr": xattr}


def edit(n, label=None, runs=None, **kw):
    return step(label or f"edit src/main.rs ({n})", runs, write={"src/main.rs": main_with(n)}, **kw)


CASES = {}


def case(name, what, files, steps, **kw):
    CASES[name] = dict(name=name, what=what, files=files, steps=steps, **kw)


# 1. A script that is not idempotent: each run is one more. A run too many or too few shows in
# the program and in the log.
case("counter", "a script that counts its runs, across two edits and a build of nothing", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=src/main.rs");
    let n = bump();
    generated(&format!("count={n}"));
}
'''), [
    step("cold build", 1),
    edit(2, runs=1),
    edit(3, runs=1),
    step("rebuild, nothing changed", 0),
])

# 2. Files watched with rerun-if-changed: one in the package, one outside it, a directory.
case("watched", "rerun-if-changed: a file in the package, one outside it, a directory", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=data.txt");
    println!("cargo:rerun-if-changed=@OUTSIDE@");
    println!("cargo:rerun-if-changed=assets");
    let mut names: Vec<_> = fs::read_dir("assets").unwrap().map(|it| it.unwrap().path()).collect();
    names.sort();
    let assets: Vec<String> = names.iter().map(|it| fs::read_to_string(it).unwrap().trim().to_owned()).collect();
    generated(&format!("in={} out={} assets={}", read("data.txt").trim(), read("@OUTSIDE@").trim(), assets.join("|")));
}
''', files={"data.txt": "one\n", "assets/a.txt": "a\n"}), [
    step("cold build", 1),
    step("edit data.txt, in the package", 1, write={"data.txt": "two\n"}),
    step("edit the file outside the package", 1, write={"@OUTSIDE@": "outside\n"}),
    step("touch data.txt, its bytes the same", 1, touch=["data.txt"]),
    step("add a file to the directory", 1, write={"assets/b.txt": "b\n"}),
    step("edit a file of the directory", 1, write={"assets/a.txt": "a2\n"}),
    step("remove the file added", 1, remove=["assets/b.txt"]),
    edit(2, "edit src/main.rs, which the script does not watch", runs=0),
    step("rebuild, nothing changed", 0),
])

# 2e. A path that is not the package's by its spelling: a link, and one that is relative and leaves
# the package. cargo reads the time of what a link points at.
case("watched-spelling", "rerun-if-changed names a link, and a path that is relative and outside the package", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=link.txt");
    println!("cargo:rerun-if-changed=../outside.txt");
    generated(&format!("link={} outside={}", read("link.txt").trim(), read("../outside.txt").trim()));
}
''', files={"target-of-link.txt": "one\n"}), [
    step("cold build", 1),
    step("edit the file the link points at", 1, write={"target-of-link.txt": "two\n"}),
    step("touch the file the link points at", 1, touch=["target-of-link.txt"]),
    step("edit the file outside, which the script names relatively", 1, write={"@OUTSIDE@": "outside\n"}),
    step("rebuild, nothing changed", 0),
], links={"link.txt": "target-of-link.txt"})

# 2f. A script that writes a file it watches, or a file of its package it names nothing of: cargo
# finds each newer than the run it made, and runs the script again for every build.
case("writes-what-it-watches", "a script that writes a file it names with rerun-if-changed", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=state.txt");
    let n: u32 = read("state.txt").trim().parse().unwrap_or(0) + 1;
    // After the tick the run began in, whose files cargo does not call newer.
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write("state.txt", n.to_string()).unwrap();
    generated(&format!("state={n}"));
}
''', files={"state.txt": "0\n"}), [
    step("cold build", 1),
    step("rebuild, nothing changed: its own file is newer than its run", 1),
    step("rebuild, nothing changed", 1),
    edit(2, runs=1),
])

case("writes-its-package", "a script that says nothing of what it reads and writes a file of its package", single(r'''
fn main() {
    note("probe");
    let n: u32 = read("written.txt").trim().parse().unwrap_or(0) + 1;
    fs::write("written.txt", n.to_string()).unwrap();
    generated(&format!("written={n}"));
}
'''), [
    step("cold build", 1),
    step("rebuild, nothing changed: a file of its package is newer than its run", 1),
    step("rebuild, nothing changed", 1),
    edit(2, runs=1),
])

# 2g. A watched directory that goes: not there, which is every build; and a script that names nothing
# and is given variables, which cargo does not run it for.
case("dir-gone", "a directory the script watches goes and comes back", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=assets");
    let n = fs::read_dir("assets").map(|it| it.count()).unwrap_or(0);
    generated(&format!("files={n}"));
}
''', files={"assets/a.txt": "a\n"}), [
    step("cold build", 1),
    step("the directory goes", 1, remove=["assets"]),
    step("rebuild, nothing changed: it is not there", 1),
    step("the directory comes back", 1, write={"assets/b.txt": "b\n"}),
    step("rebuild, nothing changed", 0),
])

case("unwatched-env", "a script that names nothing is not run for a variable", single(r'''
fn main() {
    note("probe");
    generated(&format!("var={:?}", env::var("PROBE_VAR").ok()));
}
'''), [
    step("cold build", 1),
    step("PROBE_VAR=a: not watched, so not run", 0, env={"PROBE_VAR": "a"}),
    edit(2, runs=1, env={"PROBE_VAR": "b"}),
    step("rebuild, nothing changed", 0),
])

# 2b. A watched file that is not there: cargo runs the script again for every build.
case("missing", "rerun-if-changed names a file that is not there", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=data.txt");
    println!("cargo:rerun-if-changed=absent.txt");
    generated(&format!("data={} absent={}", read("data.txt").trim(), read("absent.txt").trim()));
}
''', files={"data.txt": "one\n"}), [
    step("cold build", 1),
    step("rebuild, nothing changed", 1),
    step("the file appears", 1, write={"absent.txt": "here\n"}),
    step("rebuild, nothing changed", 0),
    step("the file goes", 1, remove=["absent.txt"]),
    step("rebuild, nothing changed", 1),
])

# 2c. The same, for a script whose output is another each run, which has the daemon load the
# workspace again: cargo runs it once a build, and so does blaze.
COUNTED = ('const EDIT: u32 = 1;\n\nfn main() {\n    println!("edit={} count={}", EDIT, env!("PROBE_COUNT"));\n}\n')
case("missing-env", "a script that runs every build and says another rustc-env each time", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=absent.txt");
    let n = bump();
    println!("cargo:rustc-env=PROBE_COUNT={n}");
    generated("unused");
}
''', files={"src/main.rs": COUNTED}), [
    step("cold build", 1),
    step("rebuild, nothing changed", 1),
    step("rebuild, nothing changed", 1),
    step("edit src/main.rs", 1, write={"src/main.rs": COUNTED.replace("EDIT: u32 = 1", "EDIT: u32 = 2")}),
])

# 2d. A build of two programs asks the daemon's worker for each: the script cargo runs every build
# is run once.
case("two-programs", "a script that runs every build, in a package of two programs", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=absent.txt");
    let n = bump();
    generated(&format!("count={n}"));
}
''', files={"src/bin/other.rs": MAIN}), [
    step("cold build", 1),
    step("rebuild, nothing changed", 1),
    edit(2, runs=1),
])

# 3. rerun-if-env-changed: set, changed, empty and unset are four values.
case("env", "rerun-if-env-changed: set, changed, empty, unset", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROBE_VAR");
    generated(&format!("var={:?}", env::var("PROBE_VAR").ok()));
}
'''), [
    step("cold build, unset", 1),
    step("PROBE_VAR=a", 1, env={"PROBE_VAR": "a"}),
    step("PROBE_VAR=a again", 0, env={"PROBE_VAR": "a"}),
    step("PROBE_VAR=b", 1, env={"PROBE_VAR": "b"}),
    step("PROBE_VAR= (empty)", 1, env={"PROBE_VAR": ""}),
    step("unset", 1),
    step("empty again", 1, env={"PROBE_VAR": ""}),
    step("empty once more", 0, env={"PROBE_VAR": ""}),
    step("unset again", 1),
    step("unset, nothing changed", 0),
])

# 3b. A variable watched from an edit of the script on.
NEWLY = r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    //WATCH
    generated(&format!("var={:?}", env::var("PROBE_NEW").ok()));
}
'''
case("env-new", "a variable the script watches only after an edit of it", single(NEWLY), [
    step("cold build, unset", 1),
    step("PROBE_NEW=1, not watched yet", 0, env={"PROBE_NEW": "1"}),
    step("edit build.rs to watch it, PROBE_NEW=1", 1, env={"PROBE_NEW": "1"},
         write={"build.rs": PRELUDE + NEWLY.replace("//WATCH", 'println!("cargo:rerun-if-env-changed=PROBE_NEW");')}),
    step("PROBE_NEW=2", 1, env={"PROBE_NEW": "2"}),
    step("PROBE_NEW=2 again", 0, env={"PROBE_NEW": "2"}),
    step("unset", 1),
])

# 3c. A variable the script starts to watch on its second run, as the verifier's did.
case("env-later", "a script that watches a variable from its second run on", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=src/main.rs");
    let n = bump();
    if n >= 2 {
        println!("cargo:rerun-if-env-changed=PROBE_NEW");
    }
    generated(&format!("count={n} var={:?}", env::var("PROBE_NEW").ok()));
}
'''), [
    step("cold build", 1),
    step("edit src/main.rs, PROBE_NEW=new", 1, env={"PROBE_NEW": "new"}, write={"src/main.rs": main_with(2)}),
    step("PROBE_NEW=newer", 1, env={"PROBE_NEW": "newer"}),
    step("PROBE_NEW=newer again", 0, env={"PROBE_NEW": "newer"}),
])

# 3f. What cargo gives the script besides the process's variables: the flags for rustc, the `[env]`
# of its configuration, the manifest, and a script that appears and goes.
case("rustflags", "RUSTFLAGS, which cargo gives every script", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    generated(&format!("flags={:?}", env::var("CARGO_ENCODED_RUSTFLAGS").ok()));
}
'''), [
    step("cold build", 1),
    step("RUSTFLAGS=--cfg a", 1, env={"RUSTFLAGS": "--cfg a"}),
    step("RUSTFLAGS=--cfg a again", 0, env={"RUSTFLAGS": "--cfg a"}),
    step("RUSTFLAGS=--cfg b", 1, env={"RUSTFLAGS": "--cfg b"}),
    step("unset: the run of the cold build is as it was, in its own directory", 0),
])

case("config-env", "[env] of cargo's configuration", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROBE_CONFIG");
    generated(&format!("config={:?}", env::var("PROBE_CONFIG").ok()));
}
''', files={".cargo/config.toml": '[env]\nPROBE_CONFIG = "a"\n'}), [
    step("cold build", 1),
    step("[env] changes", 1, write={".cargo/config.toml": '[env]\nPROBE_CONFIG = "b"\n'}),
    step("[env] is not there", 1, remove=[".cargo/config.toml"]),
    step("rebuild, nothing changed", 0),
])

case("manifest", "the manifest's version, which cargo gives every script", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    generated(&format!("version={:?}", env::var("CARGO_PKG_VERSION").ok()));
}
'''), [
    step("cold build", 1),
    step("version 0.1.1", 1, write={"Cargo.toml": MANIFEST.replace("0.1.0", "0.1.1")}),
    step("rebuild, nothing changed", 0),
    step("version 0.1.0: the run of the cold build is as it was, in its own directory", 0, write={"Cargo.toml": MANIFEST}),
])

APPEARS = ('const EDIT: u32 = 1;\n\nfn main() {\n    println!("edit={} script={:?}", EDIT, '
           'option_env!("PROBE_SCRIPT"));\n}\n')
APPEARS_SCRIPT = PRELUDE + r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=PROBE_SCRIPT=yes");
}
'''
case("script-appears", "a build.rs that appears and goes", {
    "Cargo.toml": '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    "src/main.rs": APPEARS,
}, [
    step("cold build, no script", 0),
    step("build.rs appears", 1, write={"build.rs": APPEARS_SCRIPT}),
    step("rebuild, nothing changed", 0),
    step("build.rs goes", 0, remove=["build.rs"]),
])

# 3g. What the daemon does that cargo does not: it stops, and the next build loads the workspace again;
# and what a build asks for, a profile and a feature, which is another run of the script.
case("restart", "the daemon stops between builds", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=data.txt");
    let n = bump();
    generated(&format!("count={n} data={}", read("data.txt").trim()));
}
''', files={"data.txt": "one\n"}), [
    step("cold build", 1),
    step("the daemon stops, then a build: nothing changed", 0, restart=True),
    step("the daemon stops, then a build after an edit", 1, restart=True, write={"data.txt": "two\n"}),
    step("rebuild, nothing changed", 0),
    edit(2, "edit src/main.rs", runs=0),
])

case("restart-missing", "the daemon stops between builds of a script that runs every build", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=absent.txt");
    let n = bump();
    generated(&format!("count={n}"));
}
'''), [
    step("cold build", 1),
    step("the daemon stops, then a build", 1, restart=True),
    step("rebuild", 1),
])

case("release", "builds in the release profile", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=data.txt");
    println!("cargo:rerun-if-env-changed=PROBE_VAR");
    let n = bump();
    generated(&format!("count={n} data={} var={:?} profile={}", read("data.txt").trim(), env::var("PROBE_VAR").ok(), env::var("PROFILE").unwrap()));
}
''', files={"data.txt": "one\n"}), [
    step("cold build", 1, args=["--release"]),
    step("edit data.txt", 1, args=["--release"], write={"data.txt": "two\n"}),
    step("PROBE_VAR=a", 1, args=["--release"], env={"PROBE_VAR": "a"}),
    step("rebuild, nothing changed", 0, args=["--release"], env={"PROBE_VAR": "a"}),
    edit(2, "edit src/main.rs", runs=0, args=["--release"], env={"PROBE_VAR": "a"}),
])

FEATURES = MANIFEST.replace("[workspace]", '[features]\nx = []\n\n[workspace]')
case("features", "a feature, which cargo gives the script", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    generated(&format!("x={}", env::var("CARGO_FEATURE_X").is_ok()));
}
''', files={"Cargo.toml": FEATURES}), [
    step("cold build", 1),
    step("--features x: another run of the script", 1, args=["--features", "x"]),
    step("--features x again", 0, args=["--features", "x"]),
])
# Not a step: back without the feature, cargo runs nothing (the cold build's run is fresh) and
# rebuilds the program, which blaze's whole build does not (two workers, one program path: the
# program of `--features x` stays), with no script in the project either. Found with this case.

NEW_SYNTAX = ('include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nconst EDIT: u32 = 1;\n\n'
              'fn main() {\n    println!("{} edit={} mode={}", GENERATED, EDIT, env!("PROBE_MODE"));\n}\n')
case("new-syntax", "cargo:: lines, which cargo reads as cargo: lines", single(r'''
fn main() {
    note("probe");
    println!("cargo::rerun-if-changed=mode.txt");
    println!("cargo::rerun-if-env-changed=PROBE_VAR");
    println!("cargo::rustc-env=PROBE_MODE={}", read("mode.txt").trim());
    generated(&format!("var={:?}", env::var("PROBE_VAR").ok()));
}
''', files={"mode.txt": "a\n", "src/main.rs": NEW_SYNTAX}), [
    step("cold build", 1),
    step("edit mode.txt", 1, write={"mode.txt": "b\n"}),
    step("PROBE_VAR=x", 1, env={"PROBE_VAR": "x"}),
    step("rebuild, nothing changed", 0, env={"PROBE_VAR": "x"}),
])

# 4. A script that stops saying what it reads: cargo then runs it for any file of its package.
FEW = r'''
fn main() {
    note("probe");
    //WATCH
    let n = bump();
    generated(&format!("count={n} mode=MODE"));
}
'''
case("no-directives", "a script that says nothing of what it reads after an edit of it", single(
    FEW.replace("//WATCH", 'println!("cargo:rerun-if-changed=src/main.rs");').replace("MODE", "a"),
    files={"notes.txt": "a file of the package\n"}), [
    step("cold build", 1),
    step("edit build.rs: it names nothing now", 1,
         write={"build.rs": PRELUDE + FEW.replace("//WATCH", "").replace("MODE", "b")}),
    step("rebuild, nothing changed: cargo runs it again, for what it said has changed", 1),
    step("edit a file of the package", 1, write={"notes.txt": "edited\n"}),
    edit(2, "edit src/main.rs", runs=1),
    step("rebuild, nothing changed", 0),
])

# 4b. A script that stops saying what it reads by itself, on its second run.
case("directives-dropped", "a script that drops its directives from its second run", single(r'''
fn main() {
    note("probe");
    let n = bump();
    if n == 1 {
        println!("cargo:rerun-if-changed=src/main.rs");
    }
    generated(&format!("count={n}"));
}
'''), [
    step("cold build", 1),
    edit(2, runs=1),
    step("rebuild, nothing changed: cargo runs it again, for what it said has changed", 1),
    step("rebuild, nothing changed", 0),
    edit(3, runs=1),
])

# 5. A script that fails and is fixed: cargo fails the build, and leaves what to do next to
# the next build, which runs it again.
case("fails", "a script that fails, and is fixed", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=mode.txt");
    let mode = read("mode.txt");
    if mode.trim() == "fail" {
        eprintln!("probe: the script fails");
        std::process::exit(3);
    }
    generated(&format!("mode={}", mode.trim()));
}
''', files={"mode.txt": "ok\n"}), [
    step("cold build", 1),
    step("the script fails", 1, write={"mode.txt": "fail\n"}),
    step("rebuild, nothing changed: it runs again and fails", 1),
    step("fixed", 1, write={"mode.txt": "fixed\n"}),
    step("rebuild, nothing changed", 0),
])

# 5b. A script that fails once, by how many times it ran: an edit that fails must fail, and the
# next build runs it again.
case("fails-once", "a script that fails on its third run", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=src/main.rs");
    let n = bump();
    if n == 3 {
        eprintln!("probe: the script fails on its third run");
        std::process::exit(3);
    }
    generated(&format!("count={n}"));
}
'''), [
    step("cold build", 1),
    edit(2, runs=1),
    edit(3, "edit src/main.rs: the script fails", runs=1),
    step("rebuild, nothing changed: it runs again", 1),
    step("rebuild, nothing changed", 0),
])

# 3d. What a script says to rustc, which a run changes: cfgs and variables of the crates, which the
# daemon loads the workspace again for. The script runs once, not once more for the load.
CFG = ('include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nconst EDIT: u32 = 1;\n\n'
       'fn main() {\n    println!("{} edit={} env={} a={} b={}", GENERATED, EDIT, env!("PROBE_MODE"), '
       'cfg!(mode_a), cfg!(mode_b));\n}\n')
case("cfg-env", "rustc-cfg and rustc-env that an edit of what the script reads changes", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=mode.txt");
    let mode = read("mode.txt");
    let mode = mode.trim();
    println!("cargo:rustc-cfg=mode_{mode}");
    println!("cargo:rustc-check-cfg=cfg(mode_a, mode_b)");
    println!("cargo:rustc-env=PROBE_MODE={mode}");
    generated(&format!("mode={mode}"));
}
''', files={"mode.txt": "a\n", "src/main.rs": CFG}), [
    step("cold build", 1),
    step("mode b", 1, write={"mode.txt": "b\n"}),
    step("mode a", 1, write={"mode.txt": "a\n"}),
    step("edit src/main.rs", 0, write={"src/main.rs": CFG.replace("EDIT: u32 = 1", "EDIT: u32 = 2")}),
    step("rebuild, nothing changed", 0),
])

# 3e. A line a script prints that is not UTF-8 is none of cargo's: cargo leaves it out whole, and
# the variable it set is not one.
UTF8 = ('const EDIT: u32 = 1;\n\nfn main() {\n    println!("edit={} a={:?} b={:?}", EDIT, '
        'option_env!("PROBE_A"), option_env!("PROBE_B"));\n}\n')
case("utf8", "a line a script prints that is not UTF-8", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=mode.txt");
    let mut out = std::io::stdout();
    out.write_all(b"cargo:rustc-env=PROBE_A=a\n").unwrap();
    if read("mode.txt").trim() == "bad" {
        out.write_all(b"cargo:rustc-env=PROBE_B=\xff\n").unwrap();
    } else {
        out.write_all(b"cargo:rustc-env=PROBE_B=b\n").unwrap();
    }
    generated("unused");
}
''', files={"mode.txt": "ok\n", "src/main.rs": UTF8}), [
    step("cold build", 1),
    step("the line is not UTF-8", 1, write={"mode.txt": "bad\n"}),
    step("the line is UTF-8 again", 1, write={"mode.txt": "ok\n"}),
])

# 6. A dependency with `links`: its script's metadata is the dependent script's environment, and
# a change of it, of what the dependency's script watches or of the script itself, runs the
# dependent's again.
LINKS = {
    "Cargo.toml": '[workspace]\nmembers = ["native", "probe"]\nresolver = "2"\n',
    "native/Cargo.toml": '[package]\nname = "native"\nversion = "0.1.0"\nedition = "2021"\nlinks = "probe"\nbuild = "build.rs"\n',
    "native/src/lib.rs": "",
    "native/data.txt": "1\n",
    "native/build.rs": PRELUDE + r'''
fn main() {
    note("native");
    println!("cargo:rerun-if-changed=data.txt");
    println!("cargo:val={}", read("data.txt").trim());
}
''',
    "probe/Cargo.toml": '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n'
                        '[dependencies]\nnative = { path = "../native" }\n',
    "probe/src/main.rs": MAIN,
    "probe/build.rs": PRELUDE + r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=src/main.rs");
    let n = bump();
    generated(&format!("count={n} val={:?}", env::var("DEP_PROBE_VAL").ok()));
}
''',
}
case("links", "a `links` dependency whose script's output changes, or does not", LINKS, [
    step("cold build", {"native": 1, "probe": 1}),
    step("edit native/data.txt: other metadata", {"native": 1, "probe": 1}, write={"native/data.txt": "2\n"}),
    step("edit native/data.txt: the same metadata", {"native": 1, "probe": 1}, write={"native/data.txt": "2\n\n"}),
    step("edit probe/src/main.rs", {"probe": 1}, write={"probe/src/main.rs": main_with(2)}),
    step("rebuild, nothing changed", {}),
])

# 6b. The script's own sources: a file its crate reads, and a library it is built with, are what
# cargo compiles it again for, and then it runs it again whatever it says it reads.
case("script-module", "a module of the script itself is edited", single(r'''
mod table;

fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=src/main.rs");
    generated(&format!("table={}", table::value()));
}
''', files={"table.rs": "pub fn value() -> &'static str { \"one\" }\n"}), [
    step("cold build", 1),
    step("edit table.rs, a module of the script", 1, write={"table.rs": "pub fn value() -> &'static str { \"two\" }\n"}),
    edit(2, runs=1),
    step("rebuild, nothing changed", 0),
])

BUILD_DEP = {
    "Cargo.toml": '[workspace]\nmembers = ["helper", "probe"]\nresolver = "2"\n',
    "helper/Cargo.toml": '[package]\nname = "helper"\nversion = "0.1.0"\nedition = "2021"\n',
    "helper/src/lib.rs": "pub fn value() -> &'static str { \"one\" }\n",
    "probe/Cargo.toml": '[package]\nname = "probe"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n'
                        '[build-dependencies]\nhelper = { path = "../helper" }\n',
    "probe/src/main.rs": MAIN,
    "probe/build.rs": PRELUDE + r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=src/main.rs");
    generated(&format!("helper={}", helper::value()));
}
''',
}
case("build-dep", "a build dependency of the script is edited", BUILD_DEP, [
    step("cold build", 1),
    step("edit helper/src/lib.rs, a build dependency", 1, write={"helper/src/lib.rs": "pub fn value() -> &'static str { \"two\" }\n"}),
    step("edit probe/src/main.rs", 1, write={"probe/src/main.rs": main_with(2)}),
    step("rebuild, nothing changed", 0),
])

# 7. What a script leaves in its OUT_DIR is where cargo's next run finds it: a hard link, symlinks
# (one outside), an extended attribute and a directory.
case("out-dir", "OUT_DIR with hard links, symlinks and extended attributes, left where they were", single(r'''
use std::os::unix::fs::{symlink, MetadataExt};

mod xattr {
    use std::{ffi::CString, os::raw::{c_char, c_int, c_void}, path::Path};

    extern "C" {
        fn setxattr(path: *const c_char, name: *const c_char, value: *const c_void, size: usize, flags: c_int) -> c_int;
        fn getxattr(path: *const c_char, name: *const c_char, value: *mut c_void, size: usize) -> isize;
    }

    pub fn set(path: &Path, name: &str, value: &str) -> bool {
        let path = CString::new(path.to_str().unwrap()).unwrap();
        let name = CString::new(name).unwrap();
        unsafe { setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) == 0 }
    }

    pub fn get(path: &Path, name: &str) -> Option<String> {
        let path = CString::new(path.to_str().unwrap()).unwrap();
        let name = CString::new(name).unwrap();
        let mut buffer = [0u8; 64];
        let n = unsafe { getxattr(path.as_ptr(), name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        (n >= 0).then(|| String::from_utf8_lossy(&buffer[..n as usize]).into_owned())
    }
}

fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=trigger.txt");
    let out = out();
    let (a, b, alias, external) = (out.join("a"), out.join("b"), out.join("alias"), out.join("external"));
    let n = bump();
    if n == 1 {
        fs::write(&a, "x").unwrap();
        fs::hard_link(&a, &b).unwrap();
        symlink("a", &alias).unwrap();
        symlink("@OUTSIDE@", &external).unwrap();
        let set = xattr::set(&a, "user.probe", "yes");
        fs::write(out.join("xattr-set"), set.to_string()).unwrap();
        fs::create_dir(out.join("sub")).unwrap();
        fs::write(out.join("sub").join("f"), "f").unwrap();
    }
    let same = fs::metadata(&a).unwrap().ino() == fs::metadata(&b).unwrap().ino();
    let links = fs::metadata(&a).unwrap().nlink();
    let link = fs::read_link(&alias).map(|it| it.display().to_string()).unwrap_or_else(|_| "none".to_owned());
    let attribute = xattr::get(&a, "user.probe").unwrap_or_else(|| "none".to_owned());
    let mut through = fs::OpenOptions::new().append(true).open(&external).unwrap();
    writeln!(through, "run {n}").unwrap();
    let outside = fs::read_to_string("@OUTSIDE@").unwrap();
    let sub = fs::read_to_string(out.join("sub").join("f")).unwrap_or_else(|_| "gone".to_owned());
    let set = read(out.join("xattr-set").to_str().unwrap());
    generated(&format!("count={n} same_inode={same} links={links} link={link} xattr={attribute} xattr_set={set} outside_lines={} sub={sub}", outside.lines().count()));
}
''', files={"trigger.txt": "1\n"}), [
    step("cold build", 1),
    step("edit trigger.txt", 1, write={"trigger.txt": "2\n"}),
    step("edit trigger.txt again", 1, write={"trigger.txt": "3\n"}),
    edit(2, "edit src/main.rs", runs=0),
])

# 8. A generated file the crate includes: an edit of what the script reads is the program's.
INCLUDING = ('include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nconst EDIT: u32 = 1;\n\n'
             'fn main() {\n    println!("{} edit={} table={}", GENERATED, EDIT,\n'
             '        include_str!(concat!(env!("OUT_DIR"), "/table.txt")).trim());\n}\n')
case("include", "a generated file the crate include!s, and one it include_str!s", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=template.txt");
    let template = read("template.txt");
    fs::write(out().join("table.txt"), template.to_uppercase()).unwrap();
    generated(&format!("template={}", template.trim()));
}
''', files={"template.txt": "one\n", "src/main.rs": INCLUDING}), [
    step("cold build", 1),
    step("edit template.txt", 1, write={"template.txt": "two\n"}),
    step("edit src/main.rs", 0, write={"src/main.rs": INCLUDING.replace("EDIT: u32 = 1", "EDIT: u32 = 2")}),
    step("rebuild, nothing changed", 0),
])

# 9. A run the store kept: cargo runs the script again after `cargo clean`, with whatever it does
# beside what it writes, and blaze must too, whatever its store has of the run before. The script
# counts in its OUT_DIR and leaves a line in a log, neither of which a kept run has.
case("store", "a run the store kept, and the same script run again after a clean build", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=trigger.txt");
    println!("cargo:rerun-if-env-changed=PROBE_STORE");
    let n = bump();
    generated(&format!("count={n} var={:?}", env::var("PROBE_STORE").ok()));
}
''', files={"trigger.txt": "1\n"}), [
    step("cold build", 1),
    step("clean, then build: the run of the cold build's key", 1, clean=True),
    step("PROBE_STORE=1", 1, env={"PROBE_STORE": "1"}),
    step("clean, then build with PROBE_STORE unset", 1, clean=True),
    step("clean, then build with PROBE_STORE=1", 1, clean=True, env={"PROBE_STORE": "1"}),
    step("edit trigger.txt", 1, write={"trigger.txt": "2\n"}),
    step("restore trigger.txt, clean", 1, write={"trigger.txt": "1\n"}, clean=True),
    step("rebuild, nothing changed", 0),
])


# 9. The six findings of p05w-verify, each a probe. (1) Two clients of one daemon at once are two
# builds: a script cargo runs every build (it names a file that is not there) runs for each.
case("two-clients", "two builds at once, of a script that runs every build", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=not-there.txt");
    let n = bump();
    generated(&format!("count={n}"));
}
'''), [
    step("cold build", 1),
    step("two builds at once", 2, concurrent=[{"PROBE_CLIENT": "a"}, {"PROBE_CLIENT": "b"}]),
    step("two more at once", 2, concurrent=[{"PROBE_CLIENT": "b"}, {"PROBE_CLIENT": "a"}]),
])

# (2) What the compiler read of the environment: `option_env!` in the script and in a build
# dependency, which cargo compiles the script again for, and so runs it.
COMPILER_ENV = dict(BUILD_DEP)
COMPILER_ENV["helper/src/lib.rs"] = 'pub fn value() -> &\'static str { option_env!("PROBE_DEP").unwrap_or("none") }\n'
COMPILER_ENV["probe/build.rs"] = PRELUDE + r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    let n = bump();
    generated(&format!("count={n} compiled={:?} helper={}", option_env!("PROBE_COMPILED"), helper::value()));
}
'''
case("compiler-env", "option_env! read by the script and by its build dependency", COMPILER_ENV, [
    step("cold build", 1),
    step("PROBE_COMPILED=new", 1, env={"PROBE_COMPILED": "new"}),
    step("PROBE_COMPILED=new again", 0, env={"PROBE_COMPILED": "new"}),
    step("PROBE_DEP=x as well", 1, env={"PROBE_COMPILED": "new", "PROBE_DEP": "x"}),
    step("both unset", 1),
    step("rebuild, nothing changed", 0),
])

# (3) The files of a package as cargo's PathSource lists them: a directory with a CACHEDIR.TAG is
# not left out, with or without `include`, and in git a tracked file in `target` is listed.
DATA_SCRIPT = r'''
fn main() {
    note("probe");
    let n = bump();
    generated(&format!("count={n} data={}", read("assets/data.txt").trim()));
}
'''
case("package-cachedir", "a script that names nothing, and a data file beside a CACHEDIR.TAG", single(DATA_SCRIPT, files={
    "assets/CACHEDIR.TAG": "Signature: not cargo's\n", "assets/data.txt": "one\n"}), [
    step("cold build", 1),
    step("edit assets/data.txt", 1, write={"assets/data.txt": "two\n"}),
    step("rebuild, nothing changed", 0),
])
case("package-include", "package.include names the directory with a CACHEDIR.TAG", single(DATA_SCRIPT, files={
    "Cargo.toml": MANIFEST.replace('build = "build.rs"\n', 'build = "build.rs"\ninclude = ["src/**", "build.rs", "assets/**"]\n'),
    "assets/CACHEDIR.TAG": "Signature: not cargo's\n", "assets/data.txt": "one\n", "notes.txt": "one\n"}), [
    step("cold build", 1),
    step("edit notes.txt, which include leaves out", 0, write={"notes.txt": "two\n"}),
    step("edit assets/data.txt", 1, write={"assets/data.txt": "two\n"}),
    step("rebuild, nothing changed", 0),
])
TRACKED_SCRIPT = r'''
fn main() {
    note("probe");
    let n = bump();
    generated(&format!("count={n} data={}", read("target/input.txt").trim()));
}
'''
case("package-git", "git lists the package: a tracked file in target, an ignored one, a hidden one", single(TRACKED_SCRIPT, files={
    "target/input.txt": "one\n", ".gitignore": "/target\nignored.txt\n", "ignored.txt": "one\n", ".hidden": "one\n"}), [
    step("cold build", 1),
    step("edit target/input.txt, which git tracks", 1, write={"target/input.txt": "two\n"}),
    step("edit ignored.txt, which git ignores", 0, write={"ignored.txt": "two\n"}),
    step("edit .hidden, which git lists", 1, write={".hidden": "two\n"}),
    step("rebuild, nothing changed", 0),
], git=["target/input.txt"])

# (4) The output kept of the script's last run is gone: cargo runs the script again.
case("output-gone", "the saved output of the script's last run is deleted", single(r'''
fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=build.rs");
    let n = bump();
    generated(&format!("count={n}"));
}
'''), [
    step("cold build", 1),
    step("delete the output of the run", 1, remove_output=True),
    step("rebuild, nothing changed", 0),
])

# (5) No run stands for another that differs only in what the key of a kept run leaves out: an
# extended attribute of a file in OUT_DIR, set between two runs of inputs of the same bytes.
case("out-dir-xattr", "an extended attribute in OUT_DIR changes between two runs of the same inputs", single(r'''
mod xattr {
    use std::{ffi::CString, os::raw::{c_char, c_void}, path::Path};

    extern "C" {
        fn getxattr(path: *const c_char, name: *const c_char, value: *mut c_void, size: usize) -> isize;
    }

    pub fn get(path: &Path, name: &str) -> String {
        let (path, name) = (CString::new(path.to_str().unwrap()).unwrap(), CString::new(name).unwrap());
        let mut buf = [0u8; 64];
        let n = unsafe { getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 { "none".to_owned() } else { String::from_utf8_lossy(&buf[..n as usize]).into_owned() }
    }
}

fn main() {
    note("probe");
    println!("cargo:rerun-if-changed=trigger.txt");
    let input = out().join("input");
    if !input.exists() {
        fs::write(&input, "same").unwrap();
    }
    generated(&format!("attr={}", xattr::get(&input, "user.probe")));
}
''', files={"trigger.txt": "1\n"}), [
    step("cold build", 1),
    step("set user.probe=two, touch trigger.txt", 1, touch=["trigger.txt"], xattr=("input", "user.probe", "two")),
    step("rebuild, nothing changed", 0),
])

# (6) blaze's build compiles the script and its build dependency itself, and starts no rustc,
# rustdoc or cargo: strace sees every program it starts.
case("no-compiler", "a script and a build dependency, and no rustc or cargo in blaze's build", BUILD_DEP, [
    step("cold build", 1),
    step("edit helper/src/lib.rs, a build dependency", 1, write={"helper/src/lib.rs": "pub fn value() -> &'static str { \"two\" }\n"}),
    step("edit probe/src/main.rs", 1, write={"probe/src/main.rs": main_with(2)}),
    step("rebuild, nothing changed", 0),
], trace=True)


# ---------------------------------------------------------------------------------------------


class Side:
    """One of the two builds of a case: its own copy of the project, its own log, nothing else
    shared but the store."""

    def __init__(self, kind, root, home, store, case):
        self.kind, self.case = kind, case
        self.root = os.path.join(root, kind)
        self.ws = os.path.join(self.root, "ws")
        self.log = os.path.join(self.root, "runs.log")
        self.outside = os.path.join(self.root, "outside.txt")
        self.home = home
        self.store = store

    def env(self, extra):
        env = base_env(self.home)
        env["RA_JIT_CACHE"] = self.store
        env.pop("CARGO_TARGET_DIR", None)
        for name in ("RA_JIT_MEMORY_MAX", "RA_JIT_OWN_BASE_MEMORY", "RA_JIT_SPARE"):
            if name in os.environ:
                env[name] = os.environ[name]
        if self.kind == "blaze":
            env["RA_JIT_WHOLE_BUILD"] = "1"
        else:
            env["CARGO_TARGET_DIR"] = os.path.join(self.root, "target-ref")
            env.pop("RA_JIT_WHOLE_BUILD", None)
        for name, value in extra.items():
            if value is None:
                env.pop(name, None)
            else:
                env[name] = value
        return env

    def path(self, name):
        name = name.replace("@OUTSIDE@", self.outside).replace("@LOG@", self.log)
        return name if os.path.isabs(name) else os.path.join(self.ws, name)

    def text(self, text):
        return text.replace("@OUTSIDE@", self.outside).replace("@LOG@", self.log)

    def write(self, name, text):
        path = self.path(name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(self.text(text))

    def setup(self):
        os.makedirs(self.ws, exist_ok=True)
        for name, text in self.case["files"].items():
            self.write(name, text)
        for name, target in self.case.get("links", {}).items():
            os.symlink(target, self.path(name))
        open(self.log, "w").close()
        subprocess.run(["cargo", "generate-lockfile"], cwd=self.ws, env=self.env({}), capture_output=True, check=True)
        if self.case.get("git") is not None:
            def git(*args):
                subprocess.run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost", *args],
                               cwd=self.ws, capture_output=True, check=True)
            git("init", "-q")
            git("add", "-A")
            for name in self.case["git"]:
                git("add", "-f", name)
            git("commit", "-qm", "probe")
        self.runs_read = 0

    def apply(self, s):
        time.sleep(TICK)
        if s["restart"]:
            self.stop()
        if s["clean"]:
            self.stop()
            built = os.path.join(self.ws, "target") if self.kind == "blaze" else os.path.join(self.root, "target-ref")
            shutil.rmtree(built, ignore_errors=True)
        for name, text in s["write"].items():
            self.write(name, text)
        for name in s["remove"]:
            path = self.path(name)
            if os.path.isdir(path):
                shutil.rmtree(path)
            else:
                os.remove(path)
        for name in s["touch"]:
            os.utime(self.path(name))
        if s.get("remove_output"):
            found = self.run_files("output")
            assert found, f"{self.kind} kept no output of probe's run"
            for path in found:
                os.remove(path)
        if s.get("xattr"):
            name, attr, value = s["xattr"]
            found = self.run_files(os.path.join("out", name))
            assert found, f"{self.kind} has no {name} in probe's OUT_DIR"
            for path in found:
                os.setxattr(path, attr, value.encode())

    def run_files(self, name):
        """`name` in the directories of `probe`'s runs: cargo's in its target directory, and
        blaze's in its own, below the whole build's."""
        if self.kind == "blaze":
            pattern = os.path.join(self.ws, "target", "ra-jit", "build-scripts", "debug", "build", "probe-*", name)
        else:
            pattern = os.path.join(self.root, "target-ref", "debug", "build", "probe-*", name)
        return glob.glob(pattern)

    def argv(self, args=()):
        argv = ["cargo", "ra-jit", "build"] if self.kind == "blaze" else ["cargo", "build"]
        return argv + list(args)

    def build(self, env, args=()):
        if self.kind == "blaze" and self.case.get("trace"):
            return self.traced(env, args)
        ran = subprocess.run(self.argv(args), cwd=self.ws, env=env, capture_output=True, text=True,
                             errors="replace", timeout=TIMEOUT, stdin=subprocess.DEVNULL)
        ran.execs = None
        return ran

    def traced(self, env, args=()):
        """The build under `strace -f`, with what blaze started in it. The daemon the first
        build starts stays traced by that build's strace for as long as it runs, so a build
        is over when its client has said its status, and what the daemon started for it is what
        the earlier traces gained meanwhile."""
        traces = self.__dict__.setdefault("traces", [])
        n = len(traces)
        trace, status = os.path.join(self.root, f"trace.{n}"), os.path.join(self.root, f"status.{n}")
        before = [(path, os.path.getsize(path)) for path in traces if os.path.exists(path)]
        argv = ["strace", "-f", "-qq", "-z", "-s", "4096", "-e", "trace=execve", "-o", trace,
                "sh", "-c", '"$@"; echo $? > "$0.part" && mv "$0.part" "$0"', status] + self.argv(args)
        out, err = open(trace + ".out", "w"), open(trace + ".err", "w")
        subprocess.Popen(argv, cwd=self.ws, env=env, stdout=out, stderr=err, stdin=subprocess.DEVNULL,
                         start_new_session=True)
        deadline = time.monotonic() + TIMEOUT
        while not os.path.exists(status):
            if time.monotonic() > deadline:
                raise TimeoutError(f"the traced build of {self.case['name']} did not end")
            time.sleep(0.2)
        out.close()
        err.close()
        time.sleep(1)  # what the daemon still writes of the build
        found = []
        for path, offset in before:
            found += execs(path, offset, began=True)
        found += execs(trace)
        traces.append(trace)
        ran = subprocess.CompletedProcess(argv, int(open(status).read()), open(trace + ".out").read(),
                                          open(trace + ".err", errors="replace").read())
        ran.execs = found
        return ran

    def builds(self, envs, args=()):
        """Builds with each of `envs` at once: each, once it ends."""
        started = [subprocess.Popen(self.argv(args), cwd=self.ws, env=env, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, text=True, errors="replace", stdin=subprocess.DEVNULL)
                   for env in envs]
        done = []
        for p in started:
            out, err = p.communicate(timeout=TIMEOUT)
            done.append(subprocess.CompletedProcess(p.args, p.returncode, out, err))
        return done

    def program(self, env, args=()):
        # cargo's target directory is beside the project, so that nothing of it is in the package.
        target = os.path.join(self.ws, "target") if self.kind == "blaze" else os.path.join(self.root, "target-ref")
        exe = os.path.join(target, "release" if "--release" in args else "debug", "probe")
        if not os.path.exists(exe):
            return None
        ran = subprocess.run([exe], cwd=self.ws, env=env, capture_output=True, text=True, errors="replace", timeout=60)
        return {"status": ran.returncode, "stdout": ran.stdout, "stderr": ran.stderr}

    def runs(self):
        lines = open(self.log).read().split()
        new, self.runs_read = lines[self.runs_read:], len(lines)
        counted = {}
        for line in new:
            counted[line] = counted.get(line, 0) + 1
        return counted

    def step(self, s):
        self.apply(s)
        env = self.env(s["env"])
        if s.get("concurrent"):
            each = self.builds([self.env(dict(s["env"], **it)) for it in s["concurrent"]], s["args"])
            # The step's status is the worse of the two, and it is the daemon's when both are.
            built = subprocess.CompletedProcess([], max(it.returncode for it in each), "",
                                                "\n".join(it.stderr for it in each))
            if not all("[ra-jit]" in it.stderr for it in each):
                built.stderr = built.stderr.replace("[ra-jit]", "")
            built.execs = None
        else:
            built = self.build(env, s["args"])
        result = {"build": built.returncode, "runs": self.runs(), "program": self.program(env, s["args"])}
        if built.execs is not None:
            result["execs"] = built.execs
        say = built.stderr.strip().splitlines()
        result["said"] = say[-1] if say and built.returncode != 0 else ""
        result["by_cargo"] = bool(re.search(r"building with cargo", built.stderr))
        # `[ra-jit]` is what a build the daemon made says it is: a build that ended well and does
        # not say so was cargo's, and says nothing of the whole build.
        result["daemon"] = "[ra-jit]" in built.stderr
        return result

    def stop(self):
        if self.kind == "blaze":
            subprocess.run(["cargo", "ra-jit", "stop"], cwd=self.ws, env=self.env({}), capture_output=True, timeout=120)


def execs(trace, offset=0, began=False):
    """What `strace -e trace=execve` saw started after blaze's own command began, from `offset`
    of `trace` on: each run of rustc, rustdoc or cargo (docs/no-rustc.md). The `cargo` the build
    was typed as, and rustup's proxies of it, start `cargo-ra-jit`; what starts after that is
    blaze's, and all of a trace that `began` there."""
    found = []
    with open(trace, errors="replace") as f:
        f.seek(offset)
        lines = f.read().splitlines()
    for line in lines:
        m = re.search(r'execve\("([^"]+)", \[(.*?)\]', line)
        if not m or "= -1" in line:
            continue
        name = os.path.basename(m.group(1))
        if name == "cargo-ra-jit":
            began = True
            continue
        if began and name in ("rustc", "rustdoc", "cargo"):
            found.append(f"{name} {m.group(2)[:300]}")
    return found


# What a cargo run does when its first word is one of these: it compiles (`rustc` and `fix`
# compile unless they only `--print`).
COMPILES = ("build", "b", "check", "c", "test", "t", "bench", "run", "r", "rustc", "doc", "fix", "install")


def compiles(run):
    """Whether a run `execs` found compiles: any rustc or rustdoc, a cargo that builds or
    checks. A cargo that only asks (`metadata`, `config get`, `--version`, `rustc --print`) does
    not, and is counted apart."""
    name, _, argv = run.partition(" ")
    if name != "cargo":
        return True
    words = [w.strip('"') for w in argv.split('", "')][1:]
    words = [w for w in words if not w.startswith("+")]
    while words and words[0] in ("-Z", "unstable-options", "-Zunstable-options"):
        words = words[1:]
    first = words[0] if words else ""
    return first in COMPILES and not (first == "rustc" and "--print" in words)


def differences(blaze, cargo):
    """Why blaze's step is not cargo's, as a list of sentences: empty when they are the same."""
    problems = []
    compiling = [run for run in blaze.get("execs") or [] if compiles(run)]
    if compiling:
        problems.append(f"blaze's build compiled with {len(compiling)} rustc, rustdoc or cargo: {compiling[0]}")
    if blaze["build"] == 0 and blaze.get("daemon") is False:
        problems.append("the build that succeeded was not the daemon's whole build")
    if blaze["build"] != cargo["build"]:
        problems.append(f"the build exited {blaze['build']}, cargo's {cargo['build']}")
    if blaze["runs"] != cargo["runs"]:
        problems.append(f"the scripts ran {blaze['runs'] or 'never'}, cargo's {cargo['runs'] or 'never'}")
    mine, theirs = blaze["program"], cargo["program"]
    if mine != theirs:
        if mine is None or theirs is None:
            problems.append(f"the program is {'missing' if mine is None else 'there'}, cargo's {'missing' if theirs is None else 'there'}")
        else:
            for what in ("status", "stdout", "stderr"):
                if mine[what] != theirs[what]:
                    problems.append(f"the program's {what} was {mine[what]!r}, cargo's {theirs[what]!r}")
    return problems


def expected(cargo, want):
    """Whether cargo ran what the case says it does: the case is wrong, not blaze, if not."""
    return want is None or cargo["runs"] == {k: v for k, v in want.items() if v}


def run_case(work, store, case, steps=None):
    root = os.path.join(work, case["name"])
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    sides = {kind: Side(kind, root, os.path.join(root, kind + "-home"), store, case) for kind in ("blaze", "cargo")}
    for side in sides.values():
        side.setup()
    results = []
    try:
        for s in case["steps"][:steps]:
            ours, theirs = (sides[kind].step(s) for kind in ("blaze", "cargo"))
            problems = differences(ours, theirs)
            wrong = [] if expected(theirs, s["runs"]) else [f"cargo ran {theirs['runs'] or 'nothing'}, the case says {s['runs']}"]
            results.append({"step": s["label"], "blaze": ours, "cargo": theirs, "differences": problems, "case wrong": wrong})
    finally:
        for side in sides.values():
            side.stop()
    return results


def show(case, results):
    wrong = any(r["differences"] or r["case wrong"] for r in results)
    print(f"{'WRONG' if wrong else 'ok   '} {case['name']}: {case['what']}", flush=True)
    for r in results:
        flag = "ok   " if not (r["differences"] or r["case wrong"]) else "WRONG"
        ours, theirs = r["blaze"], r["cargo"]
        runs = lambda x: ",".join(f"{k}={v}" for k, v in sorted(x["runs"].items())) or "none"
        print(f"  {flag} {r['step']:60} build {ours['build']}/{theirs['build']}  runs {runs(ours)} | {runs(theirs)}", flush=True)
        for problem in r["differences"] + r["case wrong"]:
            print(f"        {problem}", flush=True)
        if ours["by_cargo"]:
            print("        blaze said it built with cargo", flush=True)
        asking = [run for run in ours.get("execs") or [] if not compiles(run)]
        if asking:
            print(f"        blaze's build asked cargo {len(asking)} times, compiling nothing: "
                  + "; ".join(sorted({run[:90] for run in asking})), flush=True)
    return not wrong


def selftest():
    """The comparison finds what it is to: each of these is a step of blaze's against cargo's,
    and only the first is right."""
    program = lambda out, status=0: {"status": status, "stdout": out, "stderr": ""}
    base = {"build": 0, "runs": {"probe": 1}, "program": program("1 edit=2\n")}
    other = lambda **kw: dict(base, **kw)
    cases = [
        ("the same", other(), [], True),
        ("a run too many", other(runs={"probe": 2}), ["ran"], False),
        ("a run too few", other(runs={}), ["ran"], False),
        ("another script ran", other(runs={"native": 1}), ["ran"], False),
        ("the build succeeded where cargo's failed", other(), ["exited"], False),
        ("another program", other(program=program("2 edit=2\n")), ["stdout"], False),
        ("another status", other(program=program("1 edit=2\n", 1)), ["status"], False),
        ("no program", other(program=None), ["missing"], False),
        ("a build that was cargo's", other(daemon=False), ["not the daemon's"], False),
        ("a rustc blaze ran", other(execs=["rustc --crate-name build_script_build"]), ["rustc, rustdoc or cargo"], False),
        ("a cargo check blaze ran", other(execs=['cargo "/c/cargo", "check", "--compile-time-deps"']), ["rustc, rustdoc or cargo"], False),
        ("a cargo blaze only asked", other(execs=['cargo "/c/cargo", "metadata", "--no-deps"']), [], True),
    ]
    wrong = 0
    for what, ours, words, same in cases:
        theirs = base if what != "the build succeeded where cargo's failed" else other(build=101)
        found = differences(ours, theirs)
        ok = (not found) if same else all(any(w in f for f in found) for w in words) and found
        print(f"{'ok   ' if ok else 'WRONG'} {what}")
        wrong += not ok
    assert expected(base, None) and expected(base, {"probe": 1}) and not expected(base, {"probe": 2})
    assert expected(other(runs={}), {"probe": 0}) and expected(other(runs={}), {})
    if wrong:
        sys.exit(f"scriptedits.py: {wrong} selftests wrong")
    print("selftest ok")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--work")
    parser.add_argument("--json")
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--steps", type=int, help="only the first N steps of each case (for looking at one)")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("names", nargs="*")
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    if args.list:
        for case in CASES.values():
            print(f"{case['name']:20} {case['what']}")
        return
    unknown = [n for n in args.names if n not in CASES]
    if unknown:
        sys.exit(f"scriptedits.py: no case {unknown}: {', '.join(CASES)}")
    work = args.work or tempfile.mkdtemp(prefix="blaze-scriptedits-", dir="/tmp")
    os.makedirs(work, exist_ok=True)
    store = os.path.join(work, "store")
    everything, wrong = {}, []
    try:
        for name in args.names or CASES:
            results = run_case(work, store, CASES[name], args.steps)
            everything[name] = results
            if not show(CASES[name], results):
                wrong.append(name)
    finally:
        if args.json:
            os.makedirs(os.path.dirname(os.path.abspath(args.json)), exist_ok=True)
            with open(args.json, "w") as f:
                json.dump(everything, f, indent=1)
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    if wrong:
        sys.exit(f"scriptedits.py: {len(wrong)} of {len(args.names or CASES)} cases differ from cargo: {', '.join(wrong)}")
    print("scriptedits.py: every case is cargo's")


if __name__ == "__main__":
    main()
