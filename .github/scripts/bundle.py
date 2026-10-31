#!/usr/bin/env python3
"""The build bundle: a commit's ra-jit built once per platform, which every check consumes.

The contract is docs/reports/CI-fork.md ("The build bundle"). Subcommands:

  build   (build job) compile the sets of .github/ci/test-sets.json for a platform, recording
          each test process cargo would start instead of starting it
  pack    (build job) write bundle.json and bundle.tar.zst from the release binaries, the owned
          macro runtime and the sets' files
  unpack  (consumer) verify bundle.json against the digest the build job reported and the
          checkout, then unpack every listed file and verify its digest
  test    (consumer) run a test set's recorded processes as `cargo test` would
  path    (consumer) print the path of an executable a set built
  _record (cargo's runner during `build`) append one test process to the record

`test` and `path` never run cargo or rustc.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

SCHEMA = 1
HERE = Path(__file__).resolve()
RELEASE_BINS = ["ra-jit", "cargo-ra-jit", "ra-jit-rustc", "ra-jit-rustc-first"]
RUNTIME_FILES = ["blaze-proc-macro-srv", "image-pins.json", "tool-controls.json",
                 "bootstrap-audit.json", "bootstrap-external-execs.json", "build.log",
                 "bootstrap-tools.log"]
RUNTIME_GLOBS = ["*.watchdog.json", "*.exec", "*.exec.stdout", "*.exec.stderr"]
EXE = ".exe" if os.name == "nt" else ""


def die(message):
    print(f"::error::bundle: {message}", flush=True)
    sys.exit(1)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def sha256_files(*paths):
    digest = hashlib.sha256()
    for path in paths:
        digest.update(Path(path).read_bytes())
    return digest.hexdigest()


def git(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True,
                          text=True).stdout.strip()


def roots():
    return {"workspace": os.path.realpath(os.environ["GITHUB_WORKSPACE"]),
            "temp": os.path.realpath(os.environ["RUNNER_TEMP"])}


def source_identity(tree_dir):
    return {"commit": git("rev-parse", "HEAD", cwd=tree_dir),
            "tree": git("rev-parse", "HEAD^{tree}", cwd=tree_dir),
            "flake": sha256_files(Path(tree_dir) / "flake.nix", Path(tree_dir) / "flake.lock"),
            "cargo_lock": sha256_file(Path(tree_dir) / "Cargo.lock")}


def to_key(path, base):
    """`workspace/...` or `temp/...` for an absolute path under one of the roots."""
    real = os.path.realpath(path)
    for name, root in base.items():
        if real == root or real.startswith(root + os.sep):
            return name + "/" + os.path.relpath(real, root).replace(os.sep, "/")
    die(f"{path} is under neither root {base}")


def from_key(key, base):
    name, _, rest = key.partition("/")
    pieces = rest.split("/")
    if name not in base or not rest or any(part in ("", ".", "..") for part in pieces):
        die(f"refusing path {key!r}")
    root = os.path.realpath(base[name])
    path = os.path.join(root, *pieces)
    # The checkout may contain symlinks, and the tar may have been prepared on
    # another machine. Never let unpack follow one outside the assigned root.
    if os.path.commonpath([root, os.path.realpath(path)]) != root:
        die(f"refusing path outside {name}: {key!r}")
    return path


def allowed_bundle_key(key):
    """CI bundles contain build products, never checkout sources or arbitrary runner files."""
    return key.startswith("workspace/target/") or key.startswith("temp/owned-runtime/")


def artifact_path(key, base):
    """Only target artifacts may be restored; internal symlinks cannot reach source files."""
    if not allowed_bundle_key(key):
        die(f"refusing non-build artifact {key!r}")
    path = from_key(key, base)
    name, _, _ = key.partition("/")
    directory = "target" if name == "workspace" else "owned-runtime"
    allowed_root = os.path.join(os.path.realpath(base[name]), directory)
    if os.path.commonpath([allowed_root, os.path.realpath(path)]) != allowed_root:
        die(f"refusing non-build destination {key!r}")
    return path


def verified_executable(path, key, manifest):
    """Every consumer, including `path`, validates a binary before using it."""
    want = manifest["files"].get(key)
    if (not want or not want.get("executable") or not os.path.isfile(path)
            or not os.access(path, os.X_OK) or os.path.getsize(path) != want["size"]
            or sha256_file(path) != want["sha256"]):
        die(f"{path} changed since it was unpacked")
    return path


def host_triple():
    out = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, text=True).stdout
    return next(line.split()[1] for line in out.splitlines() if line.startswith("host:"))


# build ---------------------------------------------------------------------------------------

def record(argv):
    """cargo's runner: note the process cargo would start (its env as cargo adds it) and succeed."""
    base = json.loads(Path(os.environ["BUNDLE_RECORD_BASE"]).read_text())
    env = {k: v for k, v in os.environ.items()
           if base.get(k) != v and not k.startswith("BUNDLE_RECORD")}
    removed = sorted(k for k in base if k not in os.environ and not k.startswith("BUNDLE_RECORD"))
    row = {"exe": os.path.realpath(argv[0]), "args": argv[1:], "cwd": os.getcwd(),
           "env": env, "env_remove": removed}
    with open(os.environ["BUNDLE_RECORD"], "a") as f:
        f.write(json.dumps(row) + "\n")


def cargo_json(command, cwd, env):
    """Run cargo with JSON messages; the messages, and whatever else it printed, passed on."""
    print("$ " + " ".join(command), flush=True)
    proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE, text=True)
    messages = []
    for line in proc.stdout:
        if line.startswith("{"):
            try:
                messages.append(json.loads(line))
                continue
            except json.JSONDecodeError:
                pass
        sys.stdout.write(line)
    if proc.wait() != 0:
        die(f"{' '.join(command)} exited {proc.returncode}")
    return messages


def produced(messages):
    """Executables, dynamic libraries and the build-script output of local packages."""
    files, executables = set(), []
    for m in messages:
        if m.get("reason") == "compiler-artifact":
            kinds = set(m["target"]["kind"])
            if m.get("executable"):
                files.add(m["executable"])
                executables.append({"name": m["target"]["name"], "kind": sorted(kinds),
                                    "path": m["executable"]})
            if kinds & {"cdylib", "dylib"}:
                files.update(f for f in m["filenames"] if f.endswith((".so", ".dylib", ".dll")))
        elif m.get("reason") == "build-script-executed" and m["package_id"].startswith("path+"):
            out = Path(m["out_dir"])
            if out.is_dir():
                files.update(str(p) for p in out.rglob("*") if p.is_file())
    return files, executables


# Keep the C2 comparison's code-layout policy in the build job, not in a
# consumer that is forbidden to recompile. Both sides inherit the same flags.
BENCH_ALIGNMENT_FLAGS = ["-C", "llvm-args=-align-all-functions=6",
                         "-C", "llvm-args=-align-all-nofallthru-blocks=5"]


def benchmark_environment(parent):
    env = dict(parent)
    if "CARGO_ENCODED_RUSTFLAGS" in env:
        encoded = env["CARGO_ENCODED_RUSTFLAGS"]
        inherited = encoded.split("\x1f") if encoded else []
    else:
        inherited = env.get("RUSTFLAGS", "").split()
    flags = inherited + BENCH_ALIGNMENT_FLAGS
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    env.pop("RUSTFLAGS", None)
    return env, flags


def build(args):
    base = roots()
    sets = json.loads(Path(args.sets).read_text())
    triple = host_triple()
    runner_var = "CARGO_TARGET_" + triple.upper().replace("-", "_") + "_RUNNER"
    out = {}
    temp = Path(base["temp"]) / "bundle-build"
    temp.mkdir(parents=True, exist_ok=True)
    for name, spec in sets.items():
        if name.startswith("_") or args.platform not in spec["platforms"]:
            continue
        kind = next(k for k in ("test", "build", "bench") if k in spec)
        cwd = base["workspace"]
        entry = {"kind": kind, "args": spec[kind], "source": spec.get("source", "head")}
        if entry["source"] == "base":
            if not args.base_sha:
                entry.update(absent=True, reason="no base commit (not a pull request)")
                out[name] = entry
                continue
            cwd = str(Path(base["temp"]) / "base-tree")
            if not Path(cwd).exists():
                git("worktree", "add", "--detach", cwd, args.base_sha)
            entry["identity"] = source_identity(cwd)
        if spec.get("requires") and not (Path(cwd) / spec["requires"]).exists():
            entry.update(absent=True, reason=f"{spec['requires']} does not exist in {entry['source']}")
            out[name] = entry
            continue
        env = dict(os.environ)
        if entry["source"] == "base":
            # A target directory of its own beside the head's, built by the same shell and flags.
            env["CARGO_TARGET_DIR"] = str(Path(base["workspace"]) / "target" / "bundle-base")
        if kind == "test":
            recordfile = temp / f"{name}.jsonl"
            recordfile.write_text("")
            env.update({runner_var: f"{sys.executable} -I {HERE} _record",
                        "BUNDLE_RECORD": str(recordfile),
                        "BUNDLE_RECORD_BASE": str(temp / f"{name}.env.json")})
            # What cargo hands the process beyond this is what it adds, which the record keeps.
            (temp / f"{name}.env.json").write_text(json.dumps(env))
            command = ["cargo", "test", "--locked", "--message-format=json-render-diagnostics", *spec[kind]]
        elif kind == "bench":
            env, entry["rustflags"] = benchmark_environment(env)
            command = ["cargo", "bench", "--locked", "--no-run", "--message-format=json-render-diagnostics", *spec[kind]]
        else:
            command = ["cargo", "build", "--locked", "--message-format=json-render-diagnostics", *spec[kind]]
        files, executables = produced(cargo_json(command, cwd, env))
        entry["executables"] = executables
        if kind == "test":
            runs = [json.loads(line) for line in recordfile.read_text().splitlines()]
            if not runs:
                die(f"set {name} recorded no test process: `{' '.join(command)}` selects nothing")
            files.update(run["exe"] for run in runs)
            entry["runs"] = runs
        entry["files"] = sorted(to_key(f, base) for f in files)
        out[name] = entry
    Path(args.out).write_text(json.dumps(out, indent=1))


# pack ----------------------------------------------------------------------------------------

def pack(args):
    base = roots()
    ws, temp = Path(base["workspace"]), Path(base["temp"])
    sets = json.loads(Path(args.sets_record).read_text())
    keys = [f"workspace/target/release/{b}{EXE}" for b in RELEASE_BINS]
    runtime = temp / "owned-runtime"
    keys += [to_key(runtime / f, base) for f in RUNTIME_FILES if (runtime / f).is_file()]
    for pattern in RUNTIME_GLOBS:
        keys += [to_key(p, base) for p in sorted(runtime.glob(pattern))]
    if f"temp/owned-runtime/blaze-proc-macro-srv" not in keys:
        die("the owned macro runtime is missing")
    for entry in sets.values():
        keys += entry.get("files", [])
    keys = sorted(set(keys))
    files = {}
    for key in keys:
        path = artifact_path(key, base)
        if not os.path.isfile(path):
            die(f"{key} was not built")
        files[key] = {"sha256": sha256_file(path), "size": os.path.getsize(path),
                      "executable": os.access(path, os.X_OK)}

    def tool(*command):
        if not shutil.which(command[0]):
            die(f"{command[0]} is not on PATH")
        return subprocess.run(command, check=True, capture_output=True, text=True).stdout.strip()

    manifest = {
        "schema": SCHEMA,
        "platform": args.platform,
        "source": source_identity(ws),
        "toolchain": {"shell": args.shell, "rustc": tool("rustc", "-vV"),
                      "cargo": tool("cargo", "-V"), "llvm": tool("llvm-config", "--version")},
        "features": ["llvm"],
        "roots": base,
        "files": files,
        "sets": sets,
    }
    outdir = Path(args.out)
    outdir.mkdir(parents=True, exist_ok=True)
    text = json.dumps(manifest, indent=1, sort_keys=True) + "\n"
    (outdir / "bundle.json").write_text(text)
    zstd = subprocess.Popen(["zstd", "-T0", "-3", "-q", "-o", str(outdir / "bundle.tar.zst")],
                            stdin=subprocess.PIPE)
    completed = False
    try:
        # Cargo may hardlink two artifact names. The wire contract admits regular
        # members only, so pack their bytes, not tar's implicit hardlink entries.
        with tarfile.open(fileobj=zstd.stdin, mode="w|", dereference=True) as tar:
            for key in keys:
                tar.add(artifact_path(key, base), arcname=key, recursive=False)
        zstd.stdin.close()
        completed = True
    finally:
        if not completed and zstd.poll() is None:
            zstd.terminate()
        try:
            zstd.stdin.close()
        except BrokenPipeError:
            pass
        finally:
            status = zstd.wait()
    if status != 0:
        die("zstd failed")
    digest = hashlib.sha256(text.encode()).hexdigest()
    total = sum(f["size"] for f in files.values())
    print(f"bundle.json sha256 {digest}: {len(files)} files, {total / 1e6:.0f} MB")
    if args.github_output:
        with open(args.github_output, "a") as f:
            f.write(f"sha256={digest}\n")


# unpack --------------------------------------------------------------------------------------

def unpack(args):
    base = roots()
    src = Path(args.dir)
    text = (src / "bundle.json").read_bytes()
    digest = hashlib.sha256(text).hexdigest()
    if digest != args.sha256:
        die(f"bundle.json is {digest}, the build job reported {args.sha256!r}")
    manifest = json.loads(text)
    if manifest["schema"] != SCHEMA or manifest["platform"] != args.platform:
        die(f"bundle is schema {manifest['schema']} for {manifest['platform']}")
    here = source_identity(base["workspace"])
    for field in ("tree", "flake", "cargo_lock"):
        if manifest["source"][field] != here[field]:
            die(f"bundle {field} {manifest['source'][field]} differs from the checkout's {here[field]}")
    if manifest["roots"] != base:
        die(f"bundle roots {manifest['roots']} differ from this runner's {base}")
    files = manifest["files"]
    seen = set()
    zstd = subprocess.Popen(["zstd", "-d", "-c", "-q", str(src / "bundle.tar.zst")],
                            stdout=subprocess.PIPE)
    completed = False
    try:
        with tarfile.open(fileobj=zstd.stdout, mode="r|") as tar:
            for member in tar:
                want = files.get(member.name)
                if want is None or not member.isfile() or member.name in seen:
                    die(f"unlisted or repeated member {member.name!r}")
                path = artifact_path(member.name, base)
                os.makedirs(os.path.dirname(path), exist_ok=True)
                # Do not truncate a previously present hardlink or follow a final
                # symlink. A corrupt/truncated tar leaves the old file untouched.
                # The manifest's expected byte count is the limit, not a global
                # artifact size cap.
                temp_path = None
                try:
                    with tempfile.NamedTemporaryFile(dir=os.path.dirname(path),
                                                     prefix=".blaze-bundle-", delete=False) as out:
                        temp_path = out.name
                        content = hashlib.sha256()
                        size = 0
                        with tar.extractfile(member) as data:
                            for block in iter(lambda: data.read(1 << 20), b""):
                                size += len(block)
                                if size > want["size"]:
                                    die(f"{member.name} exceeds its declared size")
                                content.update(block)
                                out.write(block)
                    if content.hexdigest() != want["sha256"] or size != want["size"]:
                        die(f"{member.name} does not match bundle.json")
                    if want["executable"]:
                        os.chmod(temp_path, 0o755)
                    # Recheck in case a concurrent process changed a parent link.
                    artifact_path(member.name, base)
                    os.replace(temp_path, path)
                    temp_path = None
                finally:
                    if temp_path and os.path.exists(temp_path):
                        os.unlink(temp_path)
                seen.add(member.name)
        # tar stops at its end marker, before zstd necessarily finishes writing
        # valid padding or validating its frame checksum. Drain to EOF before
        # closing the pipe, rather than turning a valid archive into SIGPIPE.
        while zstd.stdout.read(1 << 20):
            pass
        completed = True
    finally:
        # On success let zstd exit normally; terminate it only on a rejected
        # member, when a full pipe could otherwise block the child forever.
        zstd.stdout.close()
        if not completed and zstd.poll() is None:
            zstd.terminate()
        status = zstd.wait()
    if status != 0:
        die("zstd failed")
    missing = sorted(set(files) - seen)
    if missing:
        die(f"bundle.tar.zst lacks {missing[:5]}")
    dest = Path(base["temp"]) / "bundle"
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "bundle.json").write_bytes(text)
    if args.github_env:
        with open(args.github_env, "a") as f:
            f.write(f"CI_BUNDLE_JSON={dest / 'bundle.json'}\n")
    print(f"bundle {digest[:16]} for {manifest['source']['commit']}: {len(files)} files verified")


# test, path ----------------------------------------------------------------------------------

def load_set(name):
    manifest = json.loads(Path(os.environ["CI_BUNDLE_JSON"]).read_text())
    if name not in manifest["sets"]:
        die(f"no set {name!r} in the bundle (have {sorted(manifest['sets'])})")
    return manifest, manifest["sets"][name]


def test(args):
    manifest, entry = load_set(args.set)
    if entry["kind"] != "test" or entry.get("absent"):
        die(f"set {args.set} has no test processes")
    extra = args.libtest[1:] if args.libtest[:1] == ["--"] else args.libtest
    for run in entry["runs"]:
        key = to_key(run["exe"], manifest["roots"])
        verified_executable(run["exe"], key, manifest)
        command = [run["exe"], *run["args"], *extra]
        print(f"     Running {os.path.relpath(run['exe'], manifest['roots']['workspace'])}", flush=True)
        env = dict(os.environ)
        for name in run.get("env_remove", []):
            env.pop(name, None)
        env.update(run["env"])
        code = subprocess.run(command, cwd=run["cwd"], env=env).returncode
        if code != 0:
            print(f"error: test failed, to rerun pass the same arguments to bundle.py test {args.set}")
            sys.exit(code)


def path(args):
    manifest, entry = load_set(args.set)
    if entry.get("absent"):
        print("")
        return
    found = [e["path"] for e in entry["executables"] if e["name"] == args.target]
    if len(found) != 1:
        die(f"set {args.set} built {len(found)} executables named {args.target}")
    executable = found[0]
    verified_executable(executable, to_key(executable, manifest["roots"]), manifest)
    print(executable)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "_record":
        return record(sys.argv[2:])
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--platform", required=True)
    b.add_argument("--sets", default=".github/ci/test-sets.json")
    b.add_argument("--base-sha", default="")
    b.add_argument("--out", required=True)
    k = sub.add_parser("pack")
    k.add_argument("--platform", required=True)
    k.add_argument("--shell", default="bench")
    k.add_argument("--sets-record", required=True)
    k.add_argument("--out", required=True)
    k.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    u = sub.add_parser("unpack")
    u.add_argument("--platform", required=True)
    u.add_argument("--sha256", required=True)
    u.add_argument("--dir", required=True)
    u.add_argument("--github-env", default=os.environ.get("GITHUB_ENV"))
    t = sub.add_parser("test")
    t.add_argument("set")
    t.add_argument("libtest", nargs=argparse.REMAINDER)
    q = sub.add_parser("path")
    q.add_argument("set")
    q.add_argument("target")
    args = p.parse_args()
    {"build": build, "pack": pack, "unpack": unpack, "test": test, "path": path}[args.cmd](args)


if __name__ == "__main__":
    main()
