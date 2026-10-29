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

Then the runs cargo makes one after another (`links`): a package with `links` whose script runs
again, the metadata it prints dropped and printed again (`depmeta`), and the same from a package
that is not the workspace's (`not_a_member`): a registry crate, vendored, and a path dependency
outside the workspace, which `cargo metadata --no-deps` does not list, but whose metadata cargo
gives the workspace's script all the same.

usage: scriptrerun.py --selftest    (the comparison alone, without a build)

Python 3, stdlib only.
"""
import glob, os, re, shlex, shutil, subprocess, sys, tempfile

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

QUIET = {
    "Cargo.toml": COUNTER["Cargo.toml"].replace("counter", "quiet").replace(
        'build = "build.rs"\n', 'build = "build.rs"\nexclude = ["target-ref"]\n',
    ),
    "build.rs": LOGGER["build.rs"].replace(
        '    println!("cargo::rerun-if-changed=src/main.rs");\n',
        '    println!("cargo::rustc-cfg=quiet_cfg");\n',
    ),
    "src/main.rs": COUNTER["src/main.rs"],
}

NEWENV = {
    "Cargo.toml": COUNTER["Cargo.toml"].replace("counter", "newenv"),
    "build.rs": r"""use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo::rerun-if-changed=src/main.rs");
    let main = fs::read_to_string("src/main.rs").unwrap();
    let mut n = 0;
    if main.contains("    8") {
        println!("cargo::rerun-if-env-changed=VP5_VAR");
        n = env::var("VP5_VAR").map(|v| v.len()).unwrap_or(0);
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("generated.rs"), format!("pub const RUNS: u32 = {n};\n")).unwrap();
}
""",
    "src/main.rs": COUNTER["src/main.rs"],
}

FAILING = {
    "Cargo.toml": COUNTER["Cargo.toml"].replace("counter", "failing"),
    "build.rs": r"""use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo::rerun-if-changed=src/main.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let log = out.join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
    if runs == 2 { eprintln!("second run fails"); std::process::exit(3); }
    fs::write(out.join("generated.rs"), "pub const RUNS: u32 = 0;\n").unwrap();
}
""",
    "src/main.rs": COUNTER["src/main.rs"],
}

DROPPER = {
    "Cargo.toml": COUNTER["Cargo.toml"].replace("counter", "dropper").replace(
        'build = "build.rs"\n', 'build = "build.rs"\nexclude = ["target-ref"]\n',
    ),
    "build.rs": r"""use std::{env, fs, path::PathBuf};
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let log = out.join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
    if runs == 1 { println!("cargo::rerun-if-changed=src/main.rs"); }
    fs::write(out.join("generated.rs"), format!("pub const RUNS: u32 = {runs};\n")).unwrap();
}
""",
    "src/main.rs": COUNTER["src/main.rs"],
}


def failing_script(bin_dir):
    """A failed second run ends that build. A later explicit request may retry."""
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-sf-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FAILING.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    env["RA_JIT_CACHE"] = "off"
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []
    try:
        build(side, "failing: cold", home)
        _, cold = reference.cargo("build", "-q")
        if cold.returncode:
            raise SystemExit(cold.stderr)
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"failing: not ready: {states}{tails(home)}")
        cargo_ra_jit.rewrite(os.path.join(ws, "src/main.rs"), "    7\n", "    8\n")
        for step, expected_status, expected_runs in [("an edit", 101, 2), ("explicit retry", 0, 3)]:
            _, ours = side.cargo("build")
            _, theirs = reference.cargo("build", "-q")
            counts = runs(os.path.join(ws, "target")), runs(os.path.join(ws, "target-ref"))
            if (ours.returncode, theirs.returncode) != (expected_status, expected_status):
                wrong.append(f"failing: {step}: status {ours.returncode}/{theirs.returncode}, expected {expected_status}")
            if counts != (expected_runs, expected_runs):
                wrong.append(f"failing: {step}: runs {counts}, expected {expected_runs}")
            if expected_status and "second run fails" not in ours.stderr:
                wrong.append("failing: lost the script's stderr")
            print(f"failing: {step}: status {ours.returncode}/{theirs.returncode}, runs {counts}")
    finally:
        side.cargo("stop")
        shutil.rmtree(root, ignore_errors=True)
    return wrong


def newly_watched_env(bin_dir, variable="VP5_VAR", initial=None):
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-se-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in NEWENV.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text.replace("VP5_VAR", variable))
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    if initial is None:
        env.pop(variable, None)
    else:
        env[variable] = initial
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []
    try:
        build(side, "newenv: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode != 0:
            raise SystemExit(built.stderr)
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"newenv: the daemon did not get ready: {states}{tails(home)}")
        side.env[variable] = reference.env[variable] = "new"
        cargo_ra_jit.rewrite(os.path.join(ws, "src/main.rs"), "    7\n", "    8\n")
        how = how_built(build(side, "newenv: an edit", home))
        _, built = reference.cargo("build", "-q")
        ran = subprocess.run([os.path.join(ws, "target", "debug", "newenv")], capture_output=True)
        theirs = subprocess.run([os.path.join(ws, "target-ref", "debug", "newenv")], capture_output=True)
        wrong = differences(ran, theirs, built)
        if ran.stdout != b"3 8\n":
            wrong.append(f"newenv printed {ran.stdout!r}, expected b'3 8\\n'")
        print(f"{'newenv: an edit':36} {'WRONG' if wrong else 'ok':8} {ran.stdout.strip()!r}, {how}")
    finally:
        side.cargo("stop")
        shutil.rmtree(root, ignore_errors=True)
    return wrong


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


def links_only(bin_dir):
    """A native dependency reruns with identical metadata; its parent must rerun too."""
    files = dict(COUNTER)
    files["Cargo.toml"] += '[dependencies]\nnative = { path = "native" }\n'
    files.update({
        "native/Cargo.toml": '[package]\nname="native"\nversion="0.1.0"\nedition="2021"\nlinks="probe"\n',
        "native/src/lib.rs": "pub fn value() -> u32 { 1 }\n",
        "native/build.rs": 'fn main() { println!("cargo::rerun-if-changed=data.txt"); println!("cargo::metadata=val=1"); }\n',
        "native/data.txt": "1\n",
    })
    files["build.rs"] = files["build.rs"].replace(
        '    let out = PathBuf::from(env::var("OUT_DIR").unwrap());',
        '    assert_eq!(env::var("DEP_PROBE_VAL").unwrap(), "1");\n'
        '    let out = PathBuf::from(env::var("OUT_DIR").unwrap());',
    )
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-sl-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    env["RA_JIT_CACHE"] = "off"
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []
    try:
        build(side, "links: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode:
            raise SystemExit(built.stderr)
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"links: not ready: {states}{tails(home)}")
        for value in ("2", "3"):
            cargo_ra_jit.rewrite(os.path.join(ws, "native/data.txt"), f"{int(value)-1}\n", f"{value}\n")
            how = how_built(build(side, "links: dependency edit", home))
            _, built = reference.cargo("build", "-q")
            ours = subprocess.run([os.path.join(ws, "target/debug/counter")], capture_output=True)
            theirs = subprocess.run([os.path.join(ws, "target-ref/debug/counter")], capture_output=True)
            problems = differences(ours, theirs, built)
            counts = runs(os.path.join(ws, "target")), runs(os.path.join(ws, "target-ref"))
            if counts != (int(value), int(value)):
                problems.append(f"links: runs {counts}, expected {value}")
            wrong.extend(problems)
            print(f"links: dependency edit {value}: {ours.stdout!r}/{theirs.stdout!r}, runs {counts}, {how}")
    finally:
        side.cargo("stop")
        shutil.rmtree(root, ignore_errors=True)
    return wrong


def inactive_links(bin_dir):
    """An independently built native member is not an edge of an inactive declaration."""
    wrong = []
    for kind, declaration in [
        ("optional", '[dependencies]\nnative = { path = "native", optional = true }\n'),
        ("target", '[target.\'cfg(any())\'.dependencies]\nnative = { path = "native" }\n'),
    ]:
        name = f"inactive_{kind}"
        files = dict(LOGGER)
        files["Cargo.toml"] = files["Cargo.toml"].replace("logger", name).replace(
            "[workspace]\n", '[workspace]\nmembers = ["native"]\ndefault-members = [".", "native"]\n',
        ) + declaration
        files.update({
            "native/Cargo.toml": '[package]\nname="native"\nversion="0.1.0"\nedition="2021"\nlinks="probe"\n',
            "native/src/lib.rs": "pub fn value() -> u32 { 1 }\n",
            "native/build.rs": 'fn main() { println!("cargo::rerun-if-changed=build.rs"); println!("cargo::metadata=val=1"); }\n',
        })
        wrong += [f"{step}: {why}" for step, why in counted(bin_dir, name, files, True)]
    return wrong


def continuation_edit(bin_dir):
    """An edit at Cargo's stand-in must invalidate a run completed by the daemon."""
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-cont-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    files = dict(DROPPER)
    files["Cargo.toml"] = files["Cargo.toml"].replace("dropper", "counter")
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    env["RA_JIT_CACHE"] = "off"
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []
    try:
        build(side, "continuation: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode:
            raise SystemExit(built.stderr)
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"continuation: not ready: {states}{tails(home)}")
        wrappers = glob.glob(os.path.join(ws, "target/debug/build/*/build-script-build"))
        if len(wrappers) != 1:
            raise SystemExit(f"continuation: expected one stand-in, found {wrappers}")
        wrapper = wrappers[0]
        with open(wrapper) as source:
            lines = source.read()
        if not lines.startswith("#!/bin/sh\n") or "build-script" not in lines:
            raise SystemExit(f"continuation: not a script stand-in: {wrapper}")
        # The stand-in is the file rustc linked, `build_script_build-<hash>`, and cargo makes
        # `build-script-build` of it again on every build, even of a fresh script: a hard link on
        # Linux, so an edit of either name is in both, and a copy on macOS (rust-lang/cargo#7821),
        # which replaces an edit of `build-script-build` before cargo runs it. The edit goes in
        # the file cargo makes it from.
        said = [shlex.split(line) for line in lines.splitlines() if line.startswith("RA_JIT_BUILD_SCRIPT=")]
        real = said[0][0].removeprefix("RA_JIT_BUILD_SCRIPT=") if said else ""
        linked = real.removesuffix(".real") if real.endswith(".real") else None
        if not linked or not os.path.isfile(linked):
            raise SystemExit(f"continuation: no file rustc linked the stand-in at: {lines!r}")
        # This runs after Cargo chose to invoke the stand-in, immediately before
        # production consume. It cannot affect the daemon's direct execution.
        edit = '''if test -n "$RA_JIT_SCRIPT_CONTINUATION"; then
python3 - <<'PY'
import os
from pathlib import Path
p = Path(os.environ["CARGO_MANIFEST_DIR"]) / "src/main.rs"
s = p.read_text()
assert "    8\\n" in s
p.write_text(s.replace("    8\\n", "    9\\n"))
PY
fi
'''
        with open(linked, "w") as out:
            out.write(lines.replace("#!/bin/sh\n", "#!/bin/sh\n" + edit, 1))
        source = os.path.join(ws, "src/main.rs")
        cargo_ra_jit.rewrite(source, "    7\n", "    8\n")
        # The reference executes once for the intermediate input, as judgment did.
        _, built = reference.cargo("build", "-q")
        if built.returncode:
            raise SystemExit(built.stderr)
        build(side, "continuation: edit during Cargo's leg", home)
        with open(source) as edited:
            if "    9\n" not in edited.read():
                # Without it the build compared is not the one this is about, and cargo alone,
                # which runs the script for the dropped watches anyway, is one edit ahead.
                wrong.append("continuation: cargo did not run the stand-in that edits the program")
                return wrong
        _, built = reference.cargo("build", "-q")
        ours = subprocess.run([os.path.join(ws, "target/debug/counter")], capture_output=True)
        theirs = subprocess.run([os.path.join(ws, "target-ref/debug/counter")], capture_output=True)
        wrong += differences(ours, theirs, built)
        counts = runs(os.path.join(ws, "target")), runs(os.path.join(ws, "target-ref"))
        if counts != (3, 3) or ours.stdout != b"3 9\n":
            wrong.append(f"continuation: runs {counts}, output {ours.stdout!r}, expected (3, 3) and '3 9'")
        print(f"continuation: input changed at consumption: {ours.stdout!r}/{theirs.stdout!r}, runs {counts}")
    finally:
        side.cargo("stop")
        shutil.rmtree(root, ignore_errors=True)
    return wrong


def depmeta(bin_dir):
    """A native dependency whose rerun stops printing a `cargo::metadata` key: its parent reruns
    without the `DEP_*` variable it had, as cargo runs it, and the program is the one cargo builds."""
    files = {
        "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n'
                      '[dependencies]\nnative-sys = { path = "native-sys" }\n\n[workspace]\nexclude = ["target-ref"]\n',
        "build.rs": 'use std::env;\nfn main() {\n'
                    '    println!("cargo::rerun-if-changed=src/main.rs");\n'
                    '    let features = env::var("DEP_PROBE_FEATURES").unwrap_or_else(|_| "unset".to_owned());\n'
                    '    println!("cargo::rustc-env=FEATURES={features}");\n}\n',
        "src/main.rs": 'fn main() {\n    println!("{}", env!("FEATURES"));\n}\n',
        "native-sys/Cargo.toml": '[package]\nname = "native-sys"\nversion = "0.1.0"\nedition = "2021"\n'
                                 'links = "probe"\nbuild = "build.rs"\n',
        "native-sys/src/lib.rs": "",
        "native-sys/build.rs": 'fn main() {\n    println!("cargo::rerun-if-changed=data.txt");\n'
                               '    let data = std::fs::read_to_string("data.txt").unwrap();\n'
                               '    if data.trim() == "avx" { println!("cargo::metadata=features=avx"); }\n}\n',
        "native-sys/data.txt": "avx\n",
    }
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-sd-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    env["RA_JIT_CACHE"] = "off"
    for variable in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL"):
        env.pop(variable, None)
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []
    try:
        build(side, "depmeta: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode:
            raise SystemExit(built.stderr)
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"depmeta: not ready: {states}{tails(home)}")
        for step, old, new in [("the dependency drops its key", "avx\n", "none\n"),
                               ("the dependency prints it again", "none\n", "avx\n")]:
            cargo_ra_jit.rewrite(os.path.join(ws, "native-sys/data.txt"), old, new)
            how = how_built(build(side, f"depmeta: {step}", home))
            _, built = reference.cargo("build", "-q")
            ours = subprocess.run([os.path.join(ws, "target/debug/app")], capture_output=True)
            theirs = subprocess.run([os.path.join(ws, "target-ref/debug/app")], capture_output=True)
            problems = differences(ours, theirs, built)
            wrong.extend(f"depmeta: {step}: {why}" for why in problems)
            print(f"depmeta: {step}: {ours.stdout!r}/{theirs.stdout!r}, {how}")
    finally:
        side.cargo("stop")
        shutil.rmtree(root, ignore_errors=True)
    return wrong


# A package with `links` that is not the workspace's, whose script counts its runs beside its
# `OUT_DIR` and prints what `data.txt` says as its metadata.
NOT_A_MEMBER = {
    "Cargo.toml": '[package]\nname = "{name}"\nversion = "0.1.3"\nedition = "2021"\nlinks = "probe"\nbuild = "build.rs"\n',
    "build.rs": r"""use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo::rerun-if-changed=data.txt");
    let log = PathBuf::from(env::var("OUT_DIR").unwrap()).join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
    println!("cargo::metadata=include={}", fs::read_to_string("data.txt").unwrap().trim());
}
""",
    "src/lib.rs": "",
    "data.txt": "/opt/probe\n",
}


def not_a_member(bin_dir, kind):
    """A workspace's package whose script embeds the `DEP_PROBE_INCLUDE` cargo gives it, from a
    package with `links` outside the workspace: `kind` "registry" has it from crates.io, vendored as
    a directory source that replaces it (`probe-sys`), and "outside" as a path dependency outside
    the workspace (`native`). `cargo metadata --no-deps` lists neither. Each edit of the program
    has cargo run the workspace's script again, with that metadata: the program prints the
    dependency's value as cargo's does, the build is the daemon's, and each script ran as often as
    under cargo alone. In "outside", an edit of what the dependency prints after those has cargo
    run both scripts again; the daemon does not run the dependency's, so that build is cargo's,
    and the program prints the new value. When rustc compiled the dependency's script (ra-jit
    refused it, as under memory pressure), its run is not written down, and every build after a
    change is cargo's and says why."""
    name = {"registry": "probe-sys", "outside": "native"}[kind]
    root = os.path.realpath(tempfile.mkdtemp(prefix=f"ra-jit-{kind[:3]}-", dir="/tmp"))
    ws, home, dep = os.path.join(root, "ws"), os.path.join(root, "home"), os.path.join(root, name)
    files = {
        "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n'
                      '[dependencies]\n' + ('probe-sys = "0.1"\n' if kind == "registry" else 'native = { path = "../native" }\n')
                      + '\n[workspace]\nexclude = ["target-ref"]\n',
        "build.rs": r"""use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo::rerun-if-changed=src/main.rs");
    let log = PathBuf::from(env::var("OUT_DIR").unwrap()).join("../runs");
    let runs = fs::read_to_string(&log).ok().and_then(|it| it.parse::<u32>().ok()).unwrap_or(0) + 1;
    fs::write(&log, runs.to_string()).unwrap();
    let include = env::var("DEP_PROBE_INCLUDE").unwrap_or_else(|_| "unset".into());
    println!("cargo::rustc-env=INCLUDE={include}");
}
""",
        "src/main.rs": 'fn number() -> u32 {\n    7\n}\n\nfn main() {\n    println!("{} {}", env!("INCLUDE"), number());\n}\n',
    }
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)
    vendor = os.path.join(root, "vendor")
    at = os.path.join(vendor, name) if kind == "registry" else dep
    for path, text in NOT_A_MEMBER.items():
        os.makedirs(os.path.dirname(os.path.join(at, path)), exist_ok=True)
        with open(os.path.join(at, path), "w") as out:
            out.write(text.replace("{name}", name))
    if kind == "registry":
        # What `cargo vendor` writes, without the checksums, which cargo then does not check.
        with open(os.path.join(at, ".cargo-checksum.json"), "w") as out:
            out.write('{"files":{},"package":null}')
        os.makedirs(os.path.join(ws, ".cargo"))
        with open(os.path.join(ws, ".cargo", "config.toml"), "w") as out:
            out.write(f'[source.crates-io]\nreplace-with = "vendored"\n\n[source.vendored]\ndirectory = "{vendor}"\n')
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    env["RA_JIT_CACHE"] = "off"
    for variable in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL"):
        env.pop(variable, None)
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []

    def count(target, package):
        found = glob.glob(os.path.join(ws, target, "debug", "build", f"{package}-*", "runs"))
        return sum(int(open(p).read() or 0) for p in found)

    try:
        cold = build(side, f"{kind}: cold", home)
        _, built = reference.cargo("build", "-q")
        if built.returncode:
            raise SystemExit(built.stderr)
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"{kind}: not ready: {states}{tails(home)}")
        # A script rustc compiled (ra-jit refused it, as under memory pressure) runs without the
        # stand-in, so its run is not written down: the build is cargo's, and has to say so.
        unknown = f"rustc compiles the build script of `{name}`" in cold
        edit = "cargo" if unknown else "daemon"
        if unknown:
            print(f"{kind}: rustc compiled the script of `{name}`, whose run is then not written down", flush=True)
        steps = [("an edit of the program", "ws/src/main.rs", "    7\n", "    8\n", edit),
                 ("another edit of the program", "ws/src/main.rs", "    8\n", "    9\n", edit)]
        if kind == "outside":
            steps += [("an edit of what the dependency prints", f"{name}/data.txt", "/opt/probe\n", "/opt/other\n", "cargo"),
                      ("an edit of the program after it", "ws/src/main.rs", "    9\n", "    10\n", edit)]
        for step, path, old, new, expected in steps:
            cargo_ra_jit.rewrite(os.path.join(root, path), old, new)
            how = how_built(build(side, f"{kind}: {step}", home))
            _, built = reference.cargo("build", "-q")
            ours = subprocess.run([os.path.join(ws, "target/debug/app")], capture_output=True)
            theirs = subprocess.run([os.path.join(ws, "target-ref/debug/app")], capture_output=True)
            problems = differences(ours, theirs, built)
            counts = tuple(count(t, p) for p in ("app", name) for t in ("target", "target-ref"))
            if counts[0] != counts[1] or counts[2] != counts[3]:
                problems.append(f"runs app {counts[0]}/{counts[1]}, {name} {counts[2]}/{counts[3]} (ours/cargo)")
            if how.split(":")[0] != expected:
                problems.append(f"built by {how}, expected {expected}")
            elif unknown and expected == "cargo" and "no record of" not in how and "outside the workspace" not in how:
                problems.append(f"built by {how}, not for the run the daemon has no record of")
            wrong.extend(f"{kind}: {step}: {why}" for why in problems)
            print(f"{kind}: {step}: {ours.stdout!r}/{theirs.stdout!r} exit {ours.returncode}/{theirs.returncode}, "
                  f"runs app {counts[0]}/{counts[1]} {name} {counts[2]}/{counts[3]}, {how}", flush=True)
    finally:
        side.cargo("stop")
        if wrong:
            print(f"kept {root}{tails(home, 15)}")
        else:
            shutil.rmtree(root, ignore_errors=True)
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
    for name, files, daemon in [("counter", COUNTER, False), ("logger", LOGGER, True), ("quiet", QUIET, True),
                                ("dropper", DROPPER, False)]:
        wrong += [f"{step}: {why}" for step, why in counted(bin_dir, name, files, daemon)]
    wrong += newly_watched_env(bin_dir)
    wrong += newly_watched_env(bin_dir, "DEP_USER_VALUE", "x")
    wrong += failing_script(bin_dir)
    wrong += links_only(bin_dir)
    wrong += inactive_links(bin_dir)
    wrong += continuation_edit(bin_dir)
    wrong += depmeta(bin_dir)
    wrong += not_a_member(bin_dir, "registry")
    wrong += not_a_member(bin_dir, "outside")
    import edges
    wrong += edges.run(bin_dir)
    for why in wrong:
        print(f"  {why}")
    if wrong:
        sys.exit(1)


if __name__ == "__main__":
    main()
