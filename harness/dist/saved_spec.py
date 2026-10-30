#!/usr/bin/env python3
"""Exercise saved BuildSpec execution with the real owned compiler.

The reference Cargo invocation belongs to this harness. Product compiler, Cargo,
rustdoc and rustup paths are fail/count stand-ins. A source-only fixture is saved,
its original tree is removed and the whole bundle moved, and its result must still match the
reference from an unrelated directory with hostile ambient compiler configuration, including a
config file in a directory above the replay's own. Corrupt
inputs and unsupported input closures must produce no advertised executable.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
import norustc
import tracedecay
from owned_macro_server import runtime_record


def write(root, name, contents):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents)
    return path


def blocked_toolchain(real, root, audit):
    facts = subprocess.check_output([real / "bin/rustc", "-vV"], text=True)
    host = next(line.removeprefix("host: ") for line in facts.splitlines() if line.startswith("host: "))
    name = norustc.TOOLCHAIN + "-" + host
    toolchain = root / "toolchains" / name
    (toolchain / "bin").mkdir(parents=True)
    for entry in real.iterdir():
        if entry.name not in ("bin", "libexec"):
            (toolchain / entry.name).symlink_to(entry, target_is_directory=entry.is_dir())
    for tool_name in ("rustc", "rustdoc", "cargo", "rustup", "rust-analyzer-proc-macro-srv"):
        directory = toolchain / ("libexec" if tool_name == "rust-analyzer-proc-macro-srv" else "bin")
        directory.mkdir(exist_ok=True)
        path = directory / tool_name
        path.write_text("#!" + (shutil.which("sh") or "/bin/sh") + "\nprintf '%s\\n' " + shlex.quote(tool_name)
                        + " >> " + shlex.quote(str(audit)) + "\nexit 97\n")
        path.chmod(0o755)
    write(root, "settings.toml", 'version = "12"\ndefault_toolchain = ' + json.dumps(name) + "\n")
    return toolchain


def fixture(root):
    write(root, "Cargo.toml", '[workspace]\nmembers=["app","words"]\nresolver="2"\n[profile.dev.package.saved-words]\ndebug-assertions=false\n')
    write(root, ".cargo/config.toml", '[env]\nSAVED_SPEC_LABEL={value="config-value",force=true}\n')
    write(root, "app/Cargo.toml", '''[package]
name="saved-app"
version="0.1.0"
edition="2021"
[dependencies]
renamed={package="saved-words",path="../words",default-features=false,features=["selected"]}
''')
    write(root, "app/src/main.rs", '''#[cfg(rust_analyzer)]
compile_error!("analyzer cfg entered the saved compiler request");
#[cfg(ancestor_hostile)]
compile_error!("a config file above the replay directory entered the saved compile");
fn main() {
    println!("{}|{}|{}", renamed::word(), env!("SAVED_SPEC_LABEL"),
             std::env::args().nth(1).unwrap_or_default());
}
''')
    write(root, "words/Cargo.toml", '''[package]
name="saved-words"
version="0.1.0"
edition="2021"
[features]
selected=[]
''')
    write(root, "words/src/lib.rs", '''#[cfg(not(feature="selected"))]
compile_error!("the renamed dependency lost its selected feature");
#[cfg(debug_assertions)]
compile_error!("the dependency lost its per-package debug-assertions profile");
pub fn word() -> &'static str { include_str!("../message.txt") }
''')
    write(root, "words/message.txt", "captured")
    write(root, "Cargo.lock", '''version = 4
[[package]]
name = "saved-app"
version = "0.1.0"
dependencies = ["saved-words"]
[[package]]
name = "saved-words"
version = "0.1.0"
''')


def producer_fixture(root):
    write(root, "Cargo.toml", '[workspace]\nmembers=["app","mac","words"]\nresolver="2"\n[profile.dev.package.saved-producer-words]\ndebug-assertions=false\n')
    write(root, "app/Cargo.toml", '[package]\nname="saved-producer-app"\nversion="0.1.0"\nedition="2021"\nbuild="build.rs"\n[dependencies]\nmac={package="saved-producer-mac",path="../mac"}\nwords={package="saved-producer-words",path="../words"}\n')
    write(root, "app/build.rs", 'fn main() {\nlet out=std::env::var("OUT_DIR").unwrap();\nstd::fs::write(std::path::Path::new(&out).join("generated.rs"), "const GENERATED: u32 = 5;\\n").unwrap();\nprintln!("cargo:rustc-check-cfg=cfg(owned_generated)");\nprintln!("cargo:rustc-cfg=owned_generated");\nprintln!("cargo:rustc-env=PRODUCER_LABEL=owned-build");\n}\n')
    write(root, "app/src/main.rs", '#[cfg(not(owned_generated))]\ncompile_error!("lost generated cfg");\ninclude!(concat!(env!("OUT_DIR"),"/generated.rs"));\nfn main() { println!("{}|{}", mac::answer!() + words::extra() + GENERATED, env!("PRODUCER_LABEL")); }\n')
    write(root, "mac/Cargo.toml", '[package]\nname="saved-producer-mac"\nversion="0.1.0"\nedition="2021"\n[lib]\nproc-macro=true\n')
    write(root, "mac/src/lib.rs", 'extern crate proc_macro;\n#[proc_macro]\npub fn answer(_: proc_macro::TokenStream) -> proc_macro::TokenStream { "35u32".parse().unwrap() }\n')
    write(root, "words/Cargo.toml", '[package]\nname="saved-producer-words"\nversion="0.1.0"\nedition="2021"\n')
    write(root, "words/src/lib.rs", '#[cfg(debug_assertions)]\ncompile_error!("lost package profile");\npub fn extra() -> u32 { 2 }\n')
    write(root, "Cargo.lock", 'version=4\n[[package]]\nname="saved-producer-app"\nversion="0.1.0"\ndependencies=["saved-producer-mac","saved-producer-words"]\n[[package]]\nname="saved-producer-mac"\nversion="0.1.0"\n[[package]]\nname="saved-producer-words"\nversion="0.1.0"\n')


def native_fixture(root):
    write(root, "Cargo.toml", '[package]\nname="saved-native"\nversion="0.1.0"\nedition="2021"\nbuild="build.rs"\n')
    write(root, "Cargo.lock", 'version=4\n[[package]]\nname="saved-native"\nversion="0.1.0"\n')
    write(root, "answer.c", "unsigned native_answer(void) { return 42; }\n")
    write(root, "build.rs", r'''fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let object = out.join("answer.o");
    let library = out.join("libsaved_native.a");
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    assert!(std::process::Command::new(cc).arg("-c").arg("answer.c").arg("-o").arg(&object).status().unwrap().success());
    let ar = std::env::var_os("AR").unwrap_or_else(|| "ar".into());
    assert!(std::process::Command::new(ar).arg("crs").arg(&library).arg(&object).status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=saved_native");
    println!("cargo:rerun-if-changed=answer.c");
}
''')
    write(root, "src/main.rs", 'unsafe extern "C" { fn native_answer() -> u32; }\nfn main() { println!("{}", unsafe { native_answer() }); }\n')


def dynamic_producer_fixture(root):
    write(root, "Cargo.toml", '[workspace]\nmembers=["app","native","mac"]\nresolver="2"\n')
    write(root, "Cargo.lock", '''version=4
[[package]]
name="saved-dynamic-app"
version="0.1.0"
dependencies=["saved-dynamic-macro","saved-dynamic-native"]
[[package]]
name="saved-dynamic-macro"
version="0.1.0"
dependencies=["saved-dynamic-native"]
[[package]]
name="saved-dynamic-native"
version="0.1.0"
''')
    write(root, "native/Cargo.toml", '[package]\nname="saved-dynamic-native"\nversion="0.1.0"\nedition="2021"\n')
    write(root, "native/answer.c", "unsigned saved_dynamic_answer(void) { return 42; }\n")
    write(root, "native/build.rs", r'''fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let mut command = std::process::Command::new(cc);
    if cfg!(target_os="macos") {
        command.args(["-dynamiclib", "-Wl,-install_name,libsaved_dynamic.dylib"])
            .arg("answer.c").arg("-o").arg(out.join("libsaved_dynamic.dylib"));
    } else {
        command.args(["-shared", "-fPIC", "answer.c", "-o"]).arg(out.join("libsaved_dynamic.so"));
    }
    assert!(command.status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=dylib=saved_dynamic");
    println!("cargo:rustc-env=OUT_DIR=/native-script-marker");
    println!("cargo:rerun-if-changed=answer.c");
}
''')
    write(root, "native/src/lib.rs", 'unsafe extern "C" { fn saved_dynamic_answer() -> u32; }\npub fn answer() -> u32 { assert_eq!(env!("OUT_DIR"), "/native-script-marker"); unsafe { saved_dynamic_answer() } }\n')
    write(root, "mac/Cargo.toml", '[package]\nname="saved-dynamic-macro"\nversion="0.1.0"\nedition="2021"\n[lib]\nproc-macro=true\n[dependencies]\nnative={package="saved-dynamic-native",path="../native"}\n')
    write(root, "mac/src/lib.rs", 'extern crate proc_macro;\n#[proc_macro] pub fn answer(_: proc_macro::TokenStream) -> proc_macro::TokenStream { format!("{}u32", native::answer()).parse().unwrap() }\n')
    write(root, "app/Cargo.toml", '[package]\nname="saved-dynamic-app"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nnative={package="saved-dynamic-native",path="../native"}\nmac={package="saved-dynamic-macro",path="../mac"}\n[build-dependencies]\nnative={package="saved-dynamic-native",path="../native"}\n')
    write(root, "app/build.rs", 'fn main() { let out=std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()); std::fs::write(out.join("generated.rs"), format!("const GENERATED: u32 = {};", native::answer())).unwrap(); println!("cargo:rustc-env=GENERATED_FILE={}", out.join("generated.rs").display()); println!("cargo:rustc-env=OUT_DIR=/saved-outdir-override"); println!("cargo:rustc-env=CARGO_BIN_EXE_saved-dynamic-app=source-value"); }\n')
    write(root, "app/src/main.rs", 'include!(env!("GENERATED_FILE"));\nfn main() { assert_eq!(env!("OUT_DIR"), "/saved-outdir-override"); assert_eq!(std::env::var("OUT_DIR").unwrap(), "/saved-outdir-override"); let cargo=std::path::Path::new(env!("CARGO")); println!("{}|{}|{}|{}", native::answer(), mac::answer!(), GENERATED, cargo.is_absolute() && cargo.is_file()); }\n')
    write(root, "app/tests/runtime.rs", '#[test] fn binary_environment() { assert_eq!(env!("CARGO_BIN_EXE_saved-dynamic-app"), "source-value"); let runtime=std::env::var_os("CARGO_BIN_EXE_saved-dynamic-app").unwrap(); assert!(std::path::Path::new(&runtime).is_file()); assert_ne!(runtime, std::ffi::OsString::from("source-value")); }\n')


def no_rpath_control(runner, name, files, env, cwd):
    files = sorted(set(map(Path, files)))
    assert files, "native loader control has no linked artifacts"
    tool = shutil.which("readelf" if sys.platform.startswith("linux") else "otool", path=env["PATH"])
    assert tool, "native loader control tool is unavailable"
    for index, file in enumerate(files):
        command = [tool, "-d" if sys.platform.startswith("linux") else "-l", file]
        text, _ = runner.run(f"{name}-rpath-control-{index}", command, env, cwd, guarded=False)
        assert not any(marker in text for marker in ("(RPATH)", "(RUNPATH)", "LC_RPATH")), (file, text)


def workspace_libraries(root):
    write(root, "Cargo.toml", '[workspace]\nmembers=["a","b"]\nresolver="2"\n')
    for name, value in (("a", 41), ("b", 1)):
        package = "saved-doc-" + name
        write(root, name + "/Cargo.toml", '[package]\nname=' + json.dumps(package) + '\nversion="0.1.0"\nedition="2021"\n')
        write(root, name + "/src/lib.rs", '/// ```\n/// assert_eq!(saved_doc_' + name + '::answer(), ' + str(value) + ');\n/// ```\n#[no_mangle]\npub extern "C" fn saved_' + name + '() -> u32 { answer() }\npub fn answer() -> u32 { ' + str(value) + ' }\n')
    write(root, "Cargo.lock", 'version=4\n[[package]]\nname="saved-doc-a"\nversion="0.1.0"\n[[package]]\nname="saved-doc-b"\nversion="0.1.0"\n')


def raw_compiler_case(runner, blaze, real, work, env, oracle_env):
    if sys.platform != "linux":
        return
    source = work / os.fsdecode(b"raw-source-\xff")
    root = source / "a" / os.fsdecode(b"nested-\xfe") / "src" / os.fsdecode(b"main-\xfd.rs")
    root.parent.mkdir(parents=True)
    write(root.parent, "child.rs", "pub fn answer() -> u32 { 42 }\n")
    write(source, "peer/data", "raw sibling")
    root.write_text('mod child;\nfn main() { println!("{}|{}", child::answer(), include_str!("../../../peer/data")); }\n')
    reference = work / "raw-reference"
    runner.run("raw-filename-reference-build", [real / "bin/rustc", "--edition=2021", "--crate-name", "raw_projection", root, "-o", reference], oracle_env, work, guarded=False)
    expected, _ = runner.run("raw-filename-reference-run", [reference], oracle_env, work, guarded=False)
    assert expected == "42|raw sibling\n", expected
    prefix = work / "raw-logical-namespace"

    def logical(path):
        parts = []
        for part in path.parts[1:]:
            text = ""
            for ch in part:
                if 0xDC80 <= ord(ch) <= 0xDCFF:
                    text += "~%02x" % (ord(ch) - 0xDC00)
                else:
                    text += "~7e" if ch == "~" else ch
            parts.append(text)
        return str(prefix.joinpath(*parts))

    cfg = subprocess.check_output([real / "bin/rustc", "--print", "cfg"], env=oracle_env, text=True).splitlines()
    project = {
        "compiler_mode": True, "blaze_native_projection": str(prefix),
        "sysroot": str(real), "sysroot_src": str(real / "lib/rustlib/src/rust/library"),
        "crates": [{"display_name": "raw_projection", "root_module": logical(root), "edition": "2021",
                    "deps": [], "cfg": cfg, "env": {}, "is_workspace_member": True,
                    "build": {"label": "raw_projection", "build_file": logical(root), "target_kind": "bin"}}],
    }
    descriptor = write(work, "raw-rust-project.json", "{}\n")
    binary = work / "raw-owned"
    runner.run("raw-filename-owned-build", [blaze, descriptor, "--crate", "raw_projection", "--out", work / "raw.o", "--exe", binary],
               dict(env, RA_JIT_PROJECT_STDIN="1", RA_JIT_PROC_MACRO_SERVER="0"), work,
               private_input=json.dumps(project))
    actual, _ = runner.run("raw-filename-owned-run", [binary], env, work)
    assert actual == expected, (actual, expected)
    assert not prefix.exists(), "native projection created a filesystem mirror"



def native_link_arguments(artifact):
    plan = artifact["native_link"]
    assert plan["version"] == 1 and plan["mode"] == "executable" and plan["target"], plan
    if plan["encoding"] == "unix-bytes":
        assert os.name == "posix", plan["encoding"]
        decode = lambda word: os.fsdecode(bytes(word))
    else:
        assert os.name == "nt" and plan["encoding"] == "windows-utf16le", plan["encoding"]
        decode = lambda word: bytes(word).decode("utf-16-le", "surrogatepass")
    return [decode(word) for word in plan["before"]], [decode(word) for word in plan["after"]]


def required_saved_cases(runner, args, blaze, real, work, bundle, env, oracle_env, hostile_env, hostile):
    def roundtrip(name, project, command, capture_env=env, reference_env=oracle_env, check_stdout=True):
        expected, _ = runner.run(name + "-reference", [real / "bin/cargo", *command, "--locked", "--offline"], reference_env, project, guarded=False)
        saved = bundle / (name + ".cbor")
        runner.run(name + "-save", [blaze, "cargo", "spec", "--out", saved, *command], capture_env, project)
        shutil.rmtree(project)
        actual, _ = runner.run(name + "-replay", [blaze, "cargo", "spec", "--run", saved], hostile_env, hostile)
        if check_stdout:
            assert actual == expected, (name, actual, expected)
        return saved, expected, actual

    raw_compiler_case(runner, blaze, real, work, env, oracle_env)
    profile = ["--config", "profile.dev.debug=0"]
    project = work / "producer-profile"
    producer_fixture(project)
    _, expected, _ = roundtrip("producer-profile", project, ["run", *profile])
    assert expected == "42|owned-build\n", expected

    project = work / "native-dependency"
    native_fixture(project)
    compiler = shutil.which("cc", path=oracle_env["PATH"])
    assert compiler, "native C oracle compiler is unavailable"
    wrapper = write(work, "native-tools/bound-cc", "#!" + (shutil.which("sh") or "/bin/sh") + "\nexec " + shlex.quote(compiler) + ' "$@"\n')
    wrapper.chmod(0o755)
    native_env, native_oracle = dict(env, CC=str(wrapper)), dict(oracle_env, CC=str(wrapper))
    saved, expected, _ = roundtrip("native-dependency", project, ["run", *profile], native_env, native_oracle)
    assert expected == "42\n", expected
    results = list(Path(str(saved) + ".outputs").glob("replay-*/result.json"))
    with wrapper.open("a") as output:
        output.write("# compiler bytes changed; version and behavior are unchanged\n")
    runner.run("native-compiler-bytes-changed", [blaze, "cargo", "spec", "--run", saved], hostile_env, hostile,
               expected=None, contains="platform")
    assert list(Path(str(saved) + ".outputs").glob("replay-*/result.json")) == results

    project = work / "dynamic-producers"
    dynamic_producer_fixture(project)
    dynamic_env = dict(env, NIX_DONT_SET_RPATH="1", CARGO_TARGET_DIR=str(work / "dynamic-product-target"))
    dynamic_oracle = dict(oracle_env, NIX_DONT_SET_RPATH="1", CARGO_TARGET_DIR=str(work / "dynamic-oracle-target"))
    command = ["run", "-p", "saved-dynamic-app", *profile]
    expected, _ = runner.run("dynamic-reference", [real / "bin/cargo", *command, "--locked", "--offline"], dynamic_oracle, project, guarded=False)
    assert expected == "42|42|42|true\n", expected
    extension = ".so" if sys.platform.startswith("linux") else ".dylib"
    reference_target = Path(dynamic_oracle["CARGO_TARGET_DIR"])
    controls = [reference_target / "debug/saved-dynamic-app", *reference_target.rglob("libsaved_dynamic" + extension), *reference_target.rglob("libsaved_dynamic_macro*" + extension), *reference_target.rglob("build-script-build")]
    no_rpath_control(runner, "dynamic-reference", controls, dynamic_oracle, project)
    actual, _ = runner.run("dynamic-direct", [blaze, "cargo", *command, "--locked", "--offline"], dict(dynamic_env, RA_JIT_PROC_MACRO_SERVER=str(args.owned_server)), project)
    assert actual == expected, (actual, expected)
    test_command = ["test", "-p", "saved-dynamic-app", "--test", "runtime", *profile]
    test_reference, _ = runner.run("dynamic-test-reference", [real / "bin/cargo", *test_command, "--locked", "--offline"], dynamic_oracle, project, guarded=False)
    test_direct, _ = runner.run("dynamic-test-direct", [blaze, "cargo", *test_command, "--locked", "--offline"], dict(dynamic_env, RA_JIT_PROC_MACRO_SERVER=str(args.owned_server)), project)
    from buildspec import parse_run
    assert parse_run(test_direct) == parse_run(test_reference), (test_direct, test_reference)
    test_spec = bundle / "dynamic-producer-tests.cbor"
    runner.run("dynamic-test-save", [blaze, "cargo", "spec", "--out", test_spec, *test_command], dynamic_env, project)
    saved = bundle / "dynamic-producers.cbor"
    runner.run("dynamic-save", [blaze, "cargo", "spec", "--out", saved, *command], dynamic_env, project)
    shutil.rmtree(project)
    actual, _ = runner.run("dynamic-replay", [blaze, "cargo", "spec", "--run", saved], hostile_env, hostile)
    assert actual == expected, (actual, expected)
    test_replay, _ = runner.run("dynamic-test-replay", [blaze, "cargo", "spec", "--run", test_spec], hostile_env, hostile)
    assert parse_run(test_replay) == parse_run(test_reference), (test_replay, test_reference)
    outputs = Path(str(saved) + ".outputs")
    controls = [*outputs.rglob("libsaved_dynamic" + extension), *outputs.rglob("libsaved_dynamic_macro" + extension), *outputs.rglob("build-script-build")]
    for receipt in outputs.glob("replay-*/result.json"):
        controls.extend(item["executable"] for item in json.loads(receipt.read_text())["artifacts"] if item.get("executable"))
    no_rpath_control(runner, "dynamic-product", controls, env, hostile)
    test_outputs = Path(str(test_spec) + ".outputs")
    controls = [*test_outputs.rglob("libsaved_dynamic" + extension), *test_outputs.rglob("libsaved_dynamic_macro" + extension), *test_outputs.rglob("build-script-build")]
    for receipt in test_outputs.glob("replay-*/result.json"):
        controls.extend(item["executable"] for item in json.loads(receipt.read_text())["artifacts"] if item.get("executable"))
    no_rpath_control(runner, "dynamic-test-product", controls, env, hostile)

    project = work / "workspace-libraries"
    workspace_libraries(project)
    _, _, actual = roundtrip("workspace-libraries", project, ["build", "--workspace", *profile], check_stdout=False)
    artifacts = json.loads(actual)["artifacts"]
    assert len(artifacts) == 2 and all(item["kind"] == "object" for item in artifacts), artifacts
    caller = write(work, "workspace-caller.c", "extern unsigned saved_a(void), saved_b(void);\nint main(void) { return saved_a() + saved_b() == 42 ? 0 : 1; }\n")
    binary = work / "workspace-caller"
    support = list(dict.fromkeys(item["runtime"] for item in artifacts if item.get("runtime")))
    plans = [native_link_arguments(item) for item in artifacts]
    before = [word for leading, _ in plans for word in leading]
    after = [word for _, trailing in plans for word in trailing]
    runner.run("workspace-native-link", [compiler, *before, caller, *[item["artifact"] for item in artifacts], *support, *after, "-o", binary], env, hostile)
    runner.run("workspace-native-run", [binary], env, hostile)

    project = work / "workspace-doctests"
    workspace_libraries(project)
    _, expected, actual = roundtrip("workspace-doctests", project, ["test", "--workspace", "--doc", *profile], check_stdout=False)
    from buildspec import parse_run
    assert parse_run(actual) == parse_run(expected), (actual, expected)
    assert sum(row[1] for row in parse_run(actual)["totals"]) == 2, actual


def standalone(work, name, files):
    """A one-package project below `work`, with the lock file Cargo would write for it."""
    project = work / name
    package = files["Cargo.toml"].split('name="')[1].split('"')[0]
    write(project, "Cargo.lock", 'version = 4\n[[package]]\nname = "' + package
          + '"\nversion = "0.1.0"\n')
    for path, text in files.items():
        write(project, path, text)
    return project


def saved_equals_cargo(runner, blaze, real, name, project, env, oracle_env, replay_env, hostile,
                       bundle, words=("run",)):
    """Save `project`, delete it, replay from `hostile`; the output must be Cargo's."""
    expected, _ = runner.run(name + "-reference", [real / "bin/cargo", *words, "--locked",
        "--offline"], oracle_env, project, guarded=False)
    saved = bundle / (name + ".cbor")
    runner.run(name + "-save", [blaze, "cargo", "spec", "--out", saved, *words], env, project)
    shutil.rmtree(project)
    actual, _ = runner.run(name + "-replay", [blaze, "cargo", "spec", "--run", saved],
                           replay_env, hostile)
    assert actual == expected, (name, actual, expected)
    return expected


class Runner:
    def __init__(self, binary, work, memory_mib, timeout):
        self.binary, self.work = binary, work
        self.memory_mib, self.timeout = memory_mib, timeout
        self.rows = []

    def run(self, name, command, env, cwd, expected=0, contains=None, guarded=True, private_input=None):
        label = "saved-spec-" + uuid.uuid4().hex
        env = dict(env, RA_JIT_RUN_ID=label) if guarded else dict(env)
        started = time.monotonic()
        argv = list(map(str, command))
        trace = self.work / (name + ".exec") if guarded and sys.platform == "linux" else None
        if trace:
            argv = norustc.exec_trace(argv, trace)
        child = subprocess.Popen(argv, cwd=cwd, env=env,
                                 stdin=subprocess.PIPE if private_input is not None else subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, errors="replace", start_new_session=True)
        watch = None
        if guarded:
            watch = tracedecay.Watchdog(str(self.binary.parent), self.memory_mib,
                                        tracedecay.SWAP_FREE_MIN_MB, 0.5, run_id=label,
                                        process_env=False)
            watch.client = lambda: child.pid

            def owned(rows, client_tree):
                # One-shot replay owns a new POSIX session. Session membership
                # retains reparented children without inspecting any process env.
                members = {child.pid}
                for pid, _, _, _ in rows:
                    try:
                        if os.getsid(pid) == child.pid:
                            members.add(pid)
                    except (PermissionError, ProcessLookupError):
                        pass
                return members | client_tree | tracedecay.descendants(rows, members)

            watch.pids = owned
            watch.start()
        timed_out = False
        try:
            out, err = child.communicate(input=private_input, timeout=self.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            if watch:
                rows = tracedecay.ps_table(env=False)
                tracedecay.kill_pids(watch.pids(rows, {child.pid}))
            else:
                os.killpg(child.pid, signal.SIGKILL)
            out, err = child.communicate()
        finally:
            if watch:
                watch.stop.set()
                watch.join()
        row = {"case": name, "code": child.returncode,
               "seconds": round(time.monotonic() - started, 3), "timed_out": timed_out}
        if watch:
            row.update(watch.take())
        if trace:
            row["external_execs"] = norustc.external_execs(trace.read_text())
        self.rows.append(row)
        shown = shlex.join(list(map(str, command))).encode("utf-8", "backslashreplace").decode("utf-8")
        write(self.work, name + ".log", "$ " + shown
              + "\n" + out + err)
        write(self.work, "results.json", json.dumps({"rows": self.rows}, indent=2) + "\n")
        print(f"{name}: exit={child.returncode}, seconds={row['seconds']}", flush=True)
        assert not timed_out and "watchdog" not in row, row
        assert not row.get("external_execs"), row
        if expected is None:
            assert child.returncode != 0, (name, out, err)
        else:
            assert child.returncode == expected, (name, child.returncode, out, err)
        if contains:
            assert contains in out + err, (name, contains, out, err)
        return out, err


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blaze", required=True, type=Path)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--memory-mib", type=int, default=8192)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--backend", choices=("cranelift", "llvm"), default="cranelift")
    parser.add_argument("--owned-server", type=Path, required=True,
                        help="fresh compiler-owned server; supplied only to execution, never to capture")
    parser.add_argument("--linker-library-path", help="explicit captured loader path for an externally packaged native linker")
    args = parser.parse_args()
    assert 0 < args.memory_mib <= tracedecay.WATCHDOG_MB
    assert args.timeout > 0
    blaze, work = args.blaze.resolve(), args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    owned_server = args.owned_server.resolve(strict=True)
    server_pin = runtime_record(owned_server, blaze)
    write(work, "owned-server.json", json.dumps(server_pin, indent=2) + "\n")
    # A missing process sampler must fail the guard instead of silently treating
    # every command as a zero-footprint success.
    assert any(pid == os.getpid() for pid, _, _, _ in tracedecay.ps_table(env=False)), "process sampler is unavailable"
    assert tracedecay.footprints_mb([os.getpid()]).get(os.getpid(), 0) > 0, "footprint sampler is unavailable"
    real = Path(norustc.real_toolchain())
    audit = work / "product-tools.log"
    blocked = blocked_toolchain(real, work / "rustup", audit)
    controls = []
    for name in ("rustc", "rustdoc", "cargo", "rustup", "rust-analyzer-proc-macro-srv"):
        path = blocked / ("libexec" if name == "rust-analyzer-proc-macro-srv" else "bin") / name
        control = subprocess.run([path, "--version"], capture_output=True)
        controls.append({"tool": name, "path": str(path), "code": control.returncode})
        assert control.returncode == 97, controls
    assert audit.read_text().splitlines() == [row["tool"] for row in controls], controls
    write(work, "tool-controls.json", json.dumps(controls, indent=2) + "\n")
    audit.write_text("")
    original = work / "original"
    fixture(original)
    if args.linker_library_path:
        with (original / ".cargo/config.toml").open("a") as config:
            config.write("LD_LIBRARY_PATH={value=" + json.dumps(args.linker_library_path) + ",force=true}\n")
    template = work / "fixture-template"
    shutil.copytree(original, template)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("RA_JIT_", "BLAZE_", "CARGO_", "RUSTC", "RUSTDOC"))
           and key not in ("RUSTUP_TOOLCHAIN", "RUSTFLAGS", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET")}
    native_path = os.environ.get("PATH", os.defpath)
    env.update(PATH=str(blocked / "bin") + os.pathsep + native_path,
               RUSTUP_HOME=str(work / "rustup"), HOME=str(work / "request-home"),
               CARGO_HOME=str(work / "request-cargo"), RA_JIT_CACHE="off",
               RA_JIT_MEMORY_MAX=str(args.memory_mib) + "M",
               RA_JIT_OWN_BASE_MEMORY=str(args.memory_mib * 3 // 4) + "M",
               RA_JIT_BACKEND=args.backend, RA_JIT_SPARE="0", RUST_BACKTRACE="0")
    Path(env["HOME"]).mkdir()
    Path(env["CARGO_HOME"]).mkdir()
    runner = Runner(blaze, work, args.memory_mib, args.timeout)
    bundle = work / "bundle"
    bundle.mkdir()
    selected = ["--manifest-path", "app/Cargo.toml", "--config", "profile.dev.debug=0"]
    oracle_env = {key: value for key, value in env.items()
                  if not key.startswith(("RA_JIT_", "BLAZE_"))}
    oracle_env.update(RUSTUP_HOME=str(real.parent.parent), RUSTUP_TOOLCHAIN=str(real),
                      PATH=str(real / "bin") + os.pathsep + native_path,
                      CARGO_TARGET_DIR=str(work / "oracle-target"), RUST_BACKTRACE="0")
    try:
        expected, _ = runner.run("reference", [real / "bin/cargo", "run", "--locked", "--offline",
            *selected, "--", "argument"], oracle_env, original, guarded=False)
        assert expected == "captured|config-value|argument\n", expected
        spec = bundle / "saved.cbor"
        saved, _ = runner.run("save", [blaze, "cargo", "spec", "--out", spec, "run", *selected,
                                       "--", "argument"], env, original)
        source = original / "app/src/main.rs"
        body = source.read_text()
        source.write_text(body.replace("renamed::word()", '"edited body"'))
        changed, _ = runner.run("dirty-body-save", [blaze, "cargo", "spec", "--out",
            bundle / "dirty.cbor", "run", *selected, "--", "argument"], env, original)
        before, after = json.loads(saved), json.loads(changed)
        assert before["spec"]["2"] == after["spec"]["2"], (before, after)
        assert before["digests"]["job_content"] != after["digests"]["job_content"], (before, after)
        source.write_text(body)
        # Prove the request describes the captured content, rather than HEAD or the
        # mutable checkout: change it, then remove that entire original tree.
        write(original, "words/message.txt", "mutated after capture")
        shutil.rmtree(original)
        # The bundle moves as a unit: nothing in it may name where it was made.
        moved = work / "moved-bundle"
        shutil.move(bundle, moved)
        bundle.mkdir()
        spec = moved / "saved.cbor"
        hostile = work / "unrelated"
        write(hostile, ".cargo/config.toml", '[build]\nrustflags=["--cfg", "hostile"]\n')
        # Worker-side configuration above where each replay materializes its copy: the
        # bundle directory (the moved one for this replay, the fresh one for the later
        # cases) and, farther up, the work directory.
        write(bundle, ".cargo/config.toml", '[build]\ntarget="hostile-worker-target"\n')
        write(moved, ".cargo/config.toml", '[build]\ntarget="hostile-worker-target"\n')
        ancestor = write(work, ".cargo/config.toml", '[env]\nANCESTOR_ONLY="leaked"\n[build]\nrustflags=["--cfg", "ancestor_hostile"]\n')
        hostile_env = dict(env, RUSTFLAGS="-Copt-level=invalid", RA_JIT_BACKEND="invalid",
                           CARGO_PROFILE_DEV_OPT_LEVEL="invalid", SAVED_SPEC_LABEL="ambient-value",
                           RA_JIT_PROC_MACRO_SERVER=str(owned_server))
        actual, _ = runner.run("replay-after-removal", [blaze, "cargo", "spec", "--run", spec],
                               hostile_env, hostile)
        assert actual == expected, (actual, expected)
        ancestor.unlink()
        observed = runner.rows[-1]
        assert observed["watchdog_samples"] > 0 and observed["rajit_footprint_peak_mb"] > 0, observed
        results = list(Path(str(spec) + ".outputs").glob("replay-*/result.json"))
        assert len(results) == 1, results
        artifact = json.loads(results[0].read_text())["artifacts"][0]["executable"]
        assert Path(artifact).is_file(), artifact
        if args.backend == "llvm":
            compiler_log = Path(artifact).parent / "compiler.log"
            assert any(line.startswith("backend: llvm ")
                       for line in compiler_log.read_text().splitlines()), compiler_log
        store = Path(str(spec) + ".inputs")
        source_blob = next(path for path in store.iterdir()
                           if path.is_file() and path.read_bytes() == b"captured")
        source_blob.write_bytes(b"corrupt")
        runner.run("corrupt-source", [blaze, "cargo", "spec", "--run", spec], env, hostile,
                   expected=None, contains="corrupt")
        assert list(Path(str(spec) + ".outputs").glob("replay-*/result.json")) == results
        source_blob.write_bytes(b"captured")
        # Every one of these filenames is legal on Unix, but cannot round-trip
        # through Cargo's line-oriented dep-info. Replay owns a CBOR input trace.
        project = work / "escaped-inputs"
        shutil.copytree(template, project)
        names = ["line\nbreak.txt", "carriage\rreturn.txt", "trailing\\"]
        for index, name in enumerate(names):
            write(project, "app/" + name, str(index))
        included = ",".join("include_str!(" + json.dumps("../" + name) + ")" for name in names)
        write(project, "app/src/main.rs", 'fn main() { println!("{}|{}|{}", ' + included + '); }\n')
        expected, _ = runner.run("escaped-inputs-reference", [real / "bin/cargo", "run",
            "--locked", "--offline", *selected], oracle_env, project, guarded=False)
        assert expected == "0|1|2\n", expected
        escaped = bundle / "escaped.cbor"
        runner.run("escaped-inputs-save", [blaze, "cargo", "spec", "--out", escaped,
                   "run", *selected], env, project)
        shutil.rmtree(project)
        actual, _ = runner.run("escaped-inputs-replay", [blaze, "cargo", "spec", "--run", escaped],
                               hostile_env, hostile)
        assert actual == expected, (actual, expected)
        project = work / "no-run"
        shutil.copytree(template, project)
        write(project, "app/src/main.rs", 'fn main() {}\n#[test]\nfn must_not_run() { panic!("no-run executed a test"); }\n')
        expected, _ = runner.run("no-run-reference", [real / "bin/cargo", "test", "--no-run",
            "--locked", "--offline", "--message-format=json", *selected], oracle_env,
            project, guarded=False)
        messages = [json.loads(line) for line in expected.splitlines()]
        references = [Path(message["executable"]) for message in messages
                      if message.get("reason") == "compiler-artifact"
                      and message["profile"]["test"] and message.get("executable")]
        assert len(references) == 1, references
        saved_test = bundle / "no-run.cbor"
        runner.run("no-run-save", [blaze, "cargo", "spec", "--out", saved_test,
                   "test", "--no-run", *selected], env, project)
        shutil.rmtree(project)
        actual, _ = runner.run("no-run-replay", [blaze, "cargo", "spec", "--run", saved_test],
                               hostile_env, hostile)
        results = list(Path(str(saved_test) + ".outputs").glob("replay-*/result.json"))
        assert len(results) == 1, results
        result = json.loads(results[0].read_text())
        assert json.loads(actual) == result, (actual, result)
        artifacts = result["artifacts"]
        assert len(artifacts) == 1, artifacts
        executable = Path(artifacts[0]["executable"])
        assert executable.is_file(), executable
        reference, _ = runner.run("no-run-reference-list", [references[0], "--list"],
                                  oracle_env, hostile, guarded=False)
        listed, _ = runner.run("no-run-artifact-list", [executable, "--list"], env, hostile)
        assert listed == reference, (listed, reference)
        runner.run("no-run-reference-failure", [references[0], "--exact", "must_not_run"],
                   oracle_env, hostile, expected=101, contains="test result: FAILED", guarded=False)
        runner.run("no-run-artifact-failure", [executable, "--exact", "must_not_run"], env,
                   hostile, expected=101, contains="test result: FAILED")
        # Each negative owns a fresh bundle; failed compilation/admission may not
        # leave a result that a caller could treat as a completed executable.
        project = work / "checked"
        shutil.copytree(template, project)
        runner.run("check-reference", [real / "bin/cargo", "check", "--locked", "--offline",
                   *selected], oracle_env, project, guarded=False)
        checked = bundle / "checked.cbor"
        runner.run("check-save", [blaze, "cargo", "spec", "--out", checked, "check",
                   *selected], env, project)
        shutil.rmtree(project)
        actual, _ = runner.run("check-replay", [blaze, "cargo", "spec", "--run", checked],
                               hostile_env, hostile)
        checks = json.loads(actual)["artifacts"]
        assert checks and all(artifact["kind"] == "check" and artifact["executable"] is None
                              for artifact in checks), checks
        assert all(json.loads(Path(artifact["artifact"]).read_text()) == {"checked": True}
                   for artifact in checks), checks

        project = work / "library"
        write(project, "Cargo.toml", '[package]\nname="saved-library"\nversion="0.1.0"\nedition="2021"\n')
        write(project, "Cargo.lock", 'version=4\n[[package]]\nname="saved-library"\nversion="0.1.0"\n')
        write(project, "src/lib.rs", '#[no_mangle]\npub extern "C" fn saved_answer() -> u32 { 42 }\n')
        if args.linker_library_path:
            write(project, ".cargo/config.toml", "[env]\nLD_LIBRARY_PATH={value="
                  + json.dumps(args.linker_library_path) + ",force=true}\n")
        library = bundle / "library.cbor"
        runner.run("library-save", [blaze, "cargo", "spec", "--out", library, "build",
                   "--config", "profile.dev.debug=0"], env, project)
        shutil.rmtree(project)
        actual, _ = runner.run("library-replay", [blaze, "cargo", "spec", "--run", library],
                               hostile_env, hostile)
        objects = json.loads(actual)["artifacts"]
        assert len(objects) == 1 and objects[0]["kind"] == "object", objects
        caller = write(work, "library-caller.c", "extern unsigned saved_answer(void);\nint main(void) { return saved_answer() == 42 ? 0 : 1; }\n")
        native = work / "library-caller"
        runtime = [objects[0]["runtime"]] if objects[0].get("runtime") else []
        before, after = native_link_arguments(objects[0])
        runner.run("library-native-link", ["cc", *before, caller, objects[0]["artifact"], *runtime, *after, "-o", native],
                   env, hostile)
        runner.run("library-native-run", [native], env, hostile)

        if sys.platform == "linux":
            project = work / "native-source"
            shutil.copytree(template, project)
            words = project / os.fsdecode(b"words-\xff")
            (project / "words").rename(words)
            (project / "words").symlink_to(words.name, target_is_directory=True)
            native_env = dict(env, CFLAGS=os.fsdecode(b"unused-\xff"))
            native_oracle = dict(oracle_env, CFLAGS=native_env["CFLAGS"])
            expected, _ = runner.run("native-source-reference", [real / "bin/cargo", "run",
                "--locked", "--offline", *selected, "--", "native"], native_oracle,
                project, guarded=False)
            saved_native = bundle / "native-source.cbor"
            runner.run("native-source-save", [blaze, "cargo", "spec", "--out", saved_native,
                       "run", *selected, "--", "native"], native_env, project)
            write(project, "app/src/main.rs", 'fn main() { let _ = option_env!("CFLAGS"); }\n')
            runner.run("native-env-invalid-reference", [real / "bin/cargo", "check",
                "--locked", "--offline", *selected], native_oracle, project, expected=101,
                contains="not a valid Unicode string", guarded=False)
            invalid = bundle / "native-env-invalid.cbor"
            runner.run("native-env-invalid-save", [blaze, "cargo", "spec", "--out", invalid,
                       "check", *selected], native_env, project)
            shutil.rmtree(project)
            actual, _ = runner.run("native-source-replay", [blaze, "cargo", "spec", "--run", saved_native],
                                   hostile_env, hostile)
            assert actual == expected, (actual, expected)
            runner.run("native-env-invalid-replay", [blaze, "cargo", "spec", "--run", invalid],
                       hostile_env, hostile, expected=None, contains="not a valid Unicode string")
            assert not list(Path(str(invalid) + ".outputs").glob("replay-*/result.json"))

        required_saved_cases(runner, args, blaze, real, work, bundle, env, oracle_env, hostile_env, hostile)

        for name in ("check-invalid", "external-include"):
            project = work / name
            shutil.copytree(template, project)
            if name == "check-invalid":
                write(project, "words/src/lib.rs", "pub fn word() -> u32 { false }\n")
            elif name == "external-include":
                external = write(work, "uncaptured\nexternal.txt", "outside source snapshot")
                write(project, "app/src/main.rs", 'fn main() { println!("{}", include_str!('
                      + json.dumps(str(external)) + ')); }\n')
            saved = bundle / (name + ".cbor")
            runner.run(name + "-save", [blaze, "cargo", "spec", "--out", saved,
                       "check" if name == "check-invalid" else "build", *selected], env, project)
            reason = {"check-invalid": "mismatched types", "external-include": "outside the immutable source snapshot"}[name]
            runner.run(name + "-refusal", [blaze, "cargo", "spec", "--run", saved],
                       env, hostile, expected=None, contains=reason)
            assert not list(Path(str(saved) + ".outputs").glob("replay-*/result.json")), name
        # A program sees the environment Cargo gives it, not an empty one: the package's own
        # variables, an owned CARGO frontend, and the worker's PATH and HOME.
        names = ["CARGO_MANIFEST_DIR", "CARGO_PKG_NAME", "CARGO", "PATH", "HOME"]
        project = standalone(work, "runtime-env", {
            "Cargo.toml": '[package]\nname="runtime-env"\nversion="0.1.0"\nedition="2021"\n',
            "src/main.rs": 'fn main() { for k in ' + json.dumps(names)
                           + ' { println!("{}={}", k, std::env::var_os(k).is_some()); } }\n'})
        expected = saved_equals_cargo(runner, blaze, real, "runtime-env", project, env,
                                      oracle_env, hostile_env, hostile, bundle)
        assert expected == "".join(name + "=true\n" for name in names), expected
        # option_env! of a variable the requester did not have is None for Cargo and in the
        # replay, whatever the worker has; one the requester had cannot be reproduced and is
        # refused rather than answered with a different value.
        main_rs = 'fn main() { println!("{:?}|{:?}", option_env!("SAVED_SPEC_UNSET"), option_env!("SAVED_SPEC_SET")); }\n'
        files = {"Cargo.toml": '[package]\nname="optional"\nversion="0.1.0"\nedition="2021"\n',
                 "src/main.rs": main_rs}
        project = standalone(work, "option-env-unset", files)
        expected = saved_equals_cargo(runner, blaze, real, "option-env-unset", project, env,
            oracle_env, dict(hostile_env, SAVED_SPEC_UNSET="worker-value", SAVED_SPEC_SET="worker-value"),
            hostile, bundle)
        assert expected == "None|None\n", expected
        project = standalone(work, "option-env-set", files)
        requester = dict(env, SAVED_SPEC_SET="requester-secret")
        saved = bundle / "option-env-set.cbor"
        runner.run("option-env-set-save", [blaze, "cargo", "spec", "--out", saved, "run"],
                   requester, project)
        shutil.rmtree(project)
        runner.run("option-env-set-refusal", [blaze, "cargo", "spec", "--run", saved],
                   dict(hostile_env, SAVED_SPEC_SET="worker-value"), hostile, expected=None,
                   contains="undeclared environment value SAVED_SPEC_SET")
        # A tool the requester shipped is never run by the worker: CC names a script in the
        # tree, and the platform names it by its bytes.
        marker = work / "requester-tool-ran"
        project = standalone(work, "requester-tool", {
            "Cargo.toml": '[package]\nname="requester-tool"\nversion="0.1.0"\nedition="2021"\n',
            "src/main.rs": 'fn main() { println!("built"); }\n',
            "tools/cc": "#!/bin/sh\necho ran >> " + shlex.quote(str(marker)) + "\necho cc\n"})
        (project / "tools/cc").chmod(0o755)
        expected, _ = runner.run("requester-tool-reference", [real / "bin/cargo", "run",
            "--locked", "--offline"], oracle_env, project, guarded=False)
        saved = bundle / "requester-tool.cbor"
        requester = dict(env, CC="./tools/cc")
        runner.run("requester-tool-save", [blaze, "cargo", "spec", "--out", saved, "run"],
                   requester, project)
        shutil.rmtree(project)
        actual, _ = runner.run("requester-tool-replay", [blaze, "cargo", "spec", "--run", saved],
                               dict(hostile_env, CC="./tools/cc"), hostile)
        assert actual == expected, (actual, expected)
        assert not marker.exists(), "a tool from the requester's tree was run"
        # A Cargo home that is an ancestor's .cargo (here $HOME/.cargo, with no CARGO_HOME) is
        # nearer than the files above it, and stays so in the replay.
        layers = work / "layers"
        write(layers, ".cargo/config.toml", '[env]\nLAYER_VALUE="from-above"\n')
        write(layers, "home/.cargo/config.toml", '[env]\nLAYER_VALUE="from-home"\n')
        project = standalone(layers / "home", "proj", {
            "Cargo.toml": '[package]\nname="layered"\nversion="0.1.0"\nedition="2021"\n',
            "src/main.rs": 'fn main() { println!("{}", env!("LAYER_VALUE")); }\n'})
        homed = {key: value for key, value in env.items() if key != "CARGO_HOME"}
        homed.update(HOME=str(layers / "home"))
        homed_oracle = {key: value for key, value in oracle_env.items() if key != "CARGO_HOME"}
        homed_oracle.update(HOME=str(layers / "home"))
        expected = saved_equals_cargo(runner, blaze, real, "home-layers", project, homed,
            homed_oracle, {key: value for key, value in hostile_env.items() if key != "CARGO_HOME"},
            hostile, bundle)
        assert expected == "from-home\n", expected
        print("saved source execution matches Cargo after origin removal; corrupt/external/native/unsupported inputs refused", flush=True)
    finally:
        calls = audit.read_text().splitlines()
        write(work, "results.json", json.dumps({"backend": args.backend, "rows": runner.rows,
              "tool_controls": controls, "product_tool_calls": calls,
              "owned_server": server_pin,
              "native_filename_filesystem": "tested" if sys.platform == "linux" else "not applicable: invalid Unix filename fixture requires Linux"}, indent=2) + "\n")
        assert hashlib.sha256(owned_server.read_bytes()).hexdigest() == server_pin["sha256"], "owned server changed during the gate"
        assert hashlib.sha256(blaze.read_bytes()).hexdigest() == server_pin["compiler_sha256"], "compiler changed during the gate"
        assert not calls, f"the product invoked blocked compiler tools: {calls}"


if __name__ == "__main__":
    main()
