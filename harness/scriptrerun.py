#!/usr/bin/env python3
"""scriptrerun.py: a build script that keeps what it built in its `OUT_DIR` and talks while it
builds does not send an edit to cargo when cargo would only run it again.

usage: scriptrerun.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

tracedecay's CLI has a build script that embeds a frontend. It watches every file of the
repository, so an edit to a Rust file has cargo run it again, and it runs the frontend's tool
only when the inputs it records beside the bundle in `OUT_DIR` are not those of the bundle
there. Cargo runs it again in the same `OUT_DIR`: it finds the bundle and prints directives
alone. The tool prints its own lines to the script's standard output (how long it took, which
files it wrote, where), which cargo does not read. The daemon used to run the script into an
`OUT_DIR` with nothing in it, so the tool ran, its lines were not cargo's last, and every edit
was cargo's: "the build script of `tracedecay-cli` printed something else", 18 of 18 edits
(docs/reports/BUILDSCRIPT.md).

The project here has that script in miniature: `inputs/` is what it bundles, it records a hash
of them beside the bundle, a tool (a shell) that says how long it took, which process it was
and where it wrote prints its lines when the bundle is built, and it lists every file of the
project as `rerun-if-changed`. After the first build and once the daemon has loaded the
workspace, three edits to the program are built, one of them with a line added to a file the
script only watches. Each has to be the daemon's and run as cargo's: the program exits as cargo's
reference program, built from the same source and run the same way, does, and prints byte for byte
the same to its standard output and standard error, and the reference build has to have
succeeded. A fourth, to an input the script bundles,
changes what the script writes: the program has to print the new bundle, whoever builds it.

Then two scripts that count their runs (the verifier's cases of round 3,
docs/reports/BUILDSCRIPT.md). Each watches the program's source, so each edit has cargo run it
once. One generates the count, so each run is another build, which is cargo's; the other keeps
it beside its `OUT_DIR`, where nothing compiled reads it, so the build stays the daemon's. The
daemon runs a script in cargo's `OUT_DIR` and records the run as cargo's, so it runs once for an
edit under `cargo ra-jit` too, whoever builds what it wrote, and a plain cargo build after the
edits (in a cargo home in which `ra-jit enable` was run) runs it no more than one after cargo
alone does: after one edit, after two, and after that plain build, the program prints what cargo's
reference program prints and the script ran as many times as under cargo alone.

usage: scriptrerun.py --selftest    (the comparison alone, without a build)

Python 3, stdlib only.
"""
import glob, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, how_built, wait_warm

BUILD = r'''use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
};

fn files(dir: &Path, found: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|it| it.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // Everything the project has, as tracedecay's script watches every file git lists.
    let mut watched = Vec::new();
    for dir in ["src", "inputs"] {
        files(&Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join(dir), &mut watched);
    }
    for path in &watched {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    let mut hash = DefaultHasher::new();
    let mut inputs = Vec::new();
    files(&Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("inputs"), &mut inputs);
    for path in &inputs {
        fs::read(path).unwrap().hash(&mut hash);
    }
    let built_from = hash.finish().to_string();
    let record = out.join("built-from");
    if fs::read_to_string(&record).ok().as_deref() != Some(built_from.as_str()) {
        // The tool talks on the script's standard output, which is cargo's.
        let said = Command::new("sh")
            .arg("-c")
            .arg("echo \"bundler: built in $SECONDS s by process $$\"; echo \"bundler: wrote $1\"")
            .arg("sh")
            .arg(out.join("staging"))
            .status()
            .unwrap();
        assert!(said.success());
        let mut bundle = String::new();
        for path in &inputs {
            bundle += &fs::read_to_string(path).unwrap();
        }
        fs::write(out.join("bundle.txt"), bundle).unwrap();
        fs::write(&record, &built_from).unwrap();
    }
    fs::write(out.join("generated.rs"), "pub const BUNDLE: &str = include_str!(concat!(env!(\"OUT_DIR\"), \"/bundle.txt\"));\n").unwrap();
}
'''
FILES = {
    "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n[workspace]\n',
    "build.rs": BUILD,
    "inputs/one.txt": "one\n",
    "inputs/two.txt": "two\n",
    "src/main.rs": 'include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nfn number() -> u32 {\n    7\n}\n\n'
                   'fn main() {\n    print!("{}{}", BUNDLE, number());\n    println!();\n}\n',
    "src/notes.txt": "a file the script watches and nothing compiles\n",
}


def differences(ran, theirs, built=None):
    """Why the program built (`ran`) is not what cargo's reference program (`theirs`), built from
    the same source (`built`, cargo's build of it) and run the same way, is: the reference build
    has to have succeeded, or what it is compared with says nothing, and the program's exit
    status, standard output and standard error are byte for byte what its are. Both are run
    directly, so nothing of cargo's is in what they print, and nothing is taken out of it."""
    problems = []
    if built is not None and built.returncode != 0:
        problems.append(f"the reference cargo build failed with status {built.returncode}: {built.stderr[-500:]!r}")
    for what, mine, cargos in zip(("exit status", "standard output", "standard error"),
                                  (ran.returncode, ran.stdout, ran.stderr),
                                  (theirs.returncode, theirs.stdout, theirs.stderr)):
        if mine != cargos:
            problems.append(f"the program's {what} was {mine!r}, cargo's {cargos!r}")
    return problems


def selftest():
    """The comparison finds what it is to: each of these is a program the daemon built against
    cargo's reference program, and only the first is right."""
    from types import SimpleNamespace as Ran
    ok = Ran(returncode=0, stdout="onetwo7\n", stderr="")
    built, broken = Ran(returncode=0, stderr=""), Ran(returncode=101, stderr="error: could not compile")
    nothing = Ran(returncode=101, stdout="", stderr="")
    at = lambda time: Ran(returncode=0, stdout=f"{time} onetwo7\n", stderr="")
    cases = [
        ("the same", ok, ok, built, 0),
        ("another standard output", Ran(returncode=0, stdout="onetwo8\n", stderr=""), ok, built, 1),
        ("another exit status", Ran(returncode=3, stdout="onetwo7\n", stderr=""), ok, built, 1),
        ("another standard error", Ran(returncode=0, stdout="onetwo7\n", stderr="boom\n"), ok, built, 1),
        # What `program_output` takes out as cargo's or as noise (the verifier's cases).
        ("a line on standard error that looks like a warning of cargo's",
         Ran(returncode=0, stdout="onetwo7\n", stderr="warning: boom\n"), ok, built, 1),
        ("no newline at the end", Ran(returncode=0, stdout="onetwo7", stderr=""), ok, built, 1),
        ("another time printed", at("2026-10-26T12:00:00Z"), at("2026-10-26T12:00:01Z"), built, 1),
        # Both fail and print nothing: nothing was compared.
        ("a reference build that failed, and a program that did as well", nothing, nothing, broken, 1),
        ("a reference build that failed, and a program that did not", Ran(returncode=0, stdout="", stderr=""), nothing, broken, 2),
    ]
    wrong = []
    for name, ran, theirs, building, count in cases:
        found = differences(ran, theirs, building)
        if len(found) != count:
            wrong.append(f"{name}: {len(found)} differences, not {count}: {found}")
    if wrong:
        raise SystemExit("the comparison of the program with cargo's is wrong:\n  " + "\n  ".join(wrong))


def tails(home, lines=40):
    """The last lines of each of the daemon's logs, for a failure on a machine whose files are
    gone once it ends."""
    logs = sorted(glob.glob(os.path.join(home, "*.log")) + glob.glob(os.path.join(home, "*", "*", "log")))
    return "".join(f"\n--- {log}\n" + "".join(open(log, errors="replace").readlines()[-lines:]) for log in logs)


def build(side, step, home):
    _, ran = side.cargo("build")
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{(ran.stdout + ran.stderr)[-3000:]}{tails(home)}")
    return ran.stdout + ran.stderr


# A script that counts its runs in what it generates: each run is another build.
COUNTER = {
    "Cargo.toml": '[package]\nname = "counter"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n[workspace]\n',
    "build.rs": r"""use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=src/main.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let count = out.join("count");
    let runs = fs::read_to_string(&count).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&count, runs.to_string()).unwrap();
    fs::write(out.join("generated.rs"), format!("pub const RUNS: u32 = {runs};\n")).unwrap();
    fs::write(out.join("../runs"), runs.to_string()).unwrap();
}
""",
    "src/main.rs": 'include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\nfn number() -> u32 {\n    7\n}\n\n'
                   'fn main() {\n    println!("{} {}", RUNS, number());\n}\n',
}

# A script that counts its runs beside its `OUT_DIR`, where nothing compiled reads it: every run
# writes the same, and the build stays the daemon's.
LOGGER = {
    "Cargo.toml": COUNTER["Cargo.toml"].replace("counter", "logger"),
    "build.rs": r"""use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=src/main.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let log = out.join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
    fs::write(out.join("generated.rs"), "pub const RUNS: u32 = 0;\n").unwrap();
}
""",
    "src/main.rs": COUNTER["src/main.rs"],
}


def ran_a_script(out):
    """Whether `cargo build -v` said it ran a build script."""
    return re.search(r"Running `[^`]*build-script-build", out) is not None


def runs(target):
    """How many times the script ran in `target`, by what it counts beside its `OUT_DIR`."""
    return sum(int(open(p).read() or 0) for p in glob.glob(os.path.join(target, "debug", "build", "*", "runs")))


def counted(bin_dir, name, files, daemon):
    """A script that counts its runs, built with `cargo ra-jit`, cold and with two edits, then with
    plain cargo: (step, what is wrong) for each step it is wrong in. `daemon` is whether the edits
    are the daemon's to build."""
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-sc-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    # A run the store kept would stand for the script's: what is counted here is its runs.
    env["RA_JIT_CACHE"] = "off"
    for variable in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL"):
        env.pop(variable, None)
    side, reference = Side(ws, env, True), Side(ws, env, False)
    program = os.path.join(ws, "target", "debug", name)
    reference_program = os.path.join(ws, "target-ref", "debug", name)
    wrong = []

    def compare(step, how):
        ran = subprocess.run([program], capture_output=True, timeout=60)
        theirs = subprocess.run([reference_program], capture_output=True, timeout=60)
        problems = differences(ran, theirs)
        ours, cargos = runs(os.path.join(ws, "target")), runs(os.path.join(ws, "target-ref"))
        if ours != cargos:
            problems.append(f"the script ran {ours} times, under cargo alone {cargos}")
        wrong.extend((f"{name}: {step}", why) for why in problems)
        said = ran.stdout.decode(errors="replace").strip()
        print(f"{name + ': ' + step:36} {'ok' if not problems else 'WRONG':8} {said!r}, {ours} runs, {how}")

    try:
        build(side, f"{name}: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode != 0:
            raise SystemExit(f"cargo's reference build failed: {built.stderr[-1500:]}")
        compare("cold", "")
        took, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}{tails(home)}")
        for step, old, new in [("an edit", "    7\n", "    8\n"), ("another edit", "    8\n", "    9\n")]:
            cargo_ra_jit.rewrite(os.path.join(ws, "src/main.rs"), old, new)
            out = build(side, f"{name}: {step}", home)
            _, built = reference.cargo("build", "-q")
            if built.returncode != 0:
                raise SystemExit(f"{step}: cargo's reference build failed: {built.stderr[-1500:]}")
            how = how_built(out)
            if daemon and how != "daemon":
                wrong.append((f"{name}: {step}", f"built by {how}, not the daemon"))
            compare(step, how)
        # Plain cargo in the directory `cargo ra-jit` built in, once `ra-jit enable` was run:
        # cargo's units of the package are the ones `cargo ra-jit`'s cargo built, since the
        # workspace wrapper is part of their hash, and the script ran for both edits. Cargo finds
        # it run, as cargo alone does in its own directory. (A cargo without the wrapper has units
        # of its own, which it builds and runs the script for the first time in.)
        plain_env = cargo_ra_jit.enabled_home(root, env)
        plain = subprocess.run(["cargo", "build", "-v"], cwd=ws, env=plain_env, capture_output=True, text=True, timeout=7200)
        _, built = reference.cargo("build", "-v")
        if plain.returncode != 0 or built.returncode != 0:
            raise SystemExit(f"a plain cargo build failed: {plain.stderr[-1500:]}{built.stderr[-1500:]}")
        if ran_a_script(plain.stderr) != ran_a_script(built.stderr):
            wrong.append((f"{name}: a plain cargo build", f"ran a build script: {ran_a_script(plain.stderr)}, under cargo alone {ran_a_script(built.stderr)}"))
        compare("a plain cargo build", "a script ran" if ran_a_script(plain.stderr) else "no script ran")
    finally:
        side.cargo("stop")
    if not wrong:
        shutil.rmtree(root, ignore_errors=True)
    else:
        print(f"under {root}{tails(home)}")
    return wrong


def main():
    selftest()
    if sys.argv[1:] == ["--selftest"]:
        print("selftest ok")
        return
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    # Short, for the daemon's sockets under its home.
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-sr-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL"):
        env.pop(name, None)
    side, reference = Side(ws, env, True), Side(ws, env, False)
    program = os.path.join(ws, "target", "debug", "app")
    reference_program = os.path.join(ws, "target-ref", "debug", "app")
    wrong = []
    try:
        cold = build(side, "cold", home)
        # What the script's tool said the first time, in what cargo kept: the lines cargo does not read.
        printed = glob.glob(os.path.join(ws, "target", "debug", "build", "app-*", "output")) + glob.glob(os.path.join(ws, "target", "debug", "build", "app", "*", "run", "stdout"))
        said = [open(p, errors="replace").read() for p in printed]
        if not any("bundler: built in" in s for s in said):
            raise SystemExit(f"the script's tool did not talk in the first build, which is what this tests: {printed}{cold[-1500:]}")
        took, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}{tails(home)}")
        print(f"{'ready':36} {'ok':8} {took:.1f} s")
        steps = [
            ("an edit of the program", "src/main.rs", "    7\n", "    8\n", True),
            ("an edit of a file the script watches", "src/notes.txt", "nothing compiles", "nothing compiles at all", True),
            ("another edit of the program", "src/main.rs", "    8\n", "    9\n", True),
            ("an edit of an input the script bundles", "inputs/two.txt", "two\n", "three\n", False),
        ]
        for step, path, old, new, daemon in steps:
            cargo_ra_jit.rewrite(os.path.join(ws, path), old, new)
            out = build(side, step, home)
            how = how_built(out)
            ran = subprocess.run([program], capture_output=True, timeout=60)
            _, built = reference.cargo("build", "-q")
            theirs = subprocess.run([reference_program], capture_output=True, timeout=60)
            problems = []
            # Cargo would compile the package again whatever the script printed; the daemon,
            # given the same print, has nothing to compile.
            if daemon and how != "daemon" and not how.startswith("unchanged"):
                problems.append(f"built by {how}, not the daemon")
            problems += differences(ran, theirs, built)
            if not daemon and b"three" not in ran.stdout:
                problems.append(f"the program printed {ran.stdout!r}, without the input's new text")
            wrong += [f"{step}: {why}" for why in problems]
            print(f"{step:36} {'ok' if not problems else 'WRONG':8} {how}")
    finally:
        side.cargo("stop")
    if wrong:
        print(f"under {root}{tails(home)}")
    else:
        shutil.rmtree(root, ignore_errors=True)
    for name, files, daemon in [("counter", COUNTER, False), ("logger", LOGGER, True)]:
        wrong += [f"{step}: {why}" for step, why in counted(bin_dir, name, files, daemon)]
    for why in wrong:
        print(f"  {why}")
    if wrong:
        sys.exit(1)


if __name__ == "__main__":
    main()
