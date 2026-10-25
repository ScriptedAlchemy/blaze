#!/usr/bin/env python3
"""resolve.py: what blaze-manifest reads of a workspace against what cargo reads of it, over a
corpus. cargo is the oracle; blaze never runs it outside this harness.

usage: resolve.py [PART...] [--work DIR] [--jobs N] [--blaze BIN] [--out FILE] [--limit N]
       resolve.py DIR...                 compare the workspaces at DIR... only
       resolve.py --fixtures             write the expected JSON of the fixtures with cargo
       resolve.py --top-crates N         list the N most downloaded crates.io crates, newest
                                         stable versions, into the corpus

The parts of the corpus (harness/resolve/corpus.json) are
  bench       the projects of harness/bench.py at their pinned revisions
  crates      crates.io crates, each its `.crate` unpacked as a package of its own
  workspaces  workspace repositories at pinned commits
and every part runs when none is named. Each is fetched under `--work` (default
$TMPDIR/blaze-resolve) with a CARGO_HOME of its own there, then `cargo metadata --no-deps
--format-version 1` and the `metadata` example of blaze-manifest are run on it from its root.
Both outputs are normalized (the root's absolute path and CARGO_HOME replaced, so that runs on
different machines compare) and compared as text, key order included. A workspace is counted
equal, different, blaze-failed, cargo-failed or both-failed (both refused it, with cargo's error
text or not), and the first difference of each that is not equal is printed. `--out` writes every case and its class as
JSON; the counts are docs/resolve.md's.

`--fixtures` runs cargo on each fixture of crates/blaze-manifest/tests/fixtures, copied into a
directory of its own, from the directory its `fixture.json` names in `cwd` (its root if none),
and writes what it printed, normalized, as the fixture's `expected.json`, or `{"error": ...}`
with the error's lines when cargo refused it: the crate's tests compare against those without
cargo or the network. The cargo is `cargo
+1.99.0` unless CARGO says otherwise; the blaze-manifest example is built with it unless
`--blaze` names one.
"""
import concurrent.futures, json, os, re, shutil, subprocess, sys, tarfile, tempfile, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CORPUS = os.path.join(HERE, "resolve", "corpus.json")
FIXTURES = os.path.join(ROOT, "crates", "blaze-manifest", "tests", "fixtures")
CARGO = os.environ.get("CARGO_ORACLE", "cargo +1.99.0").split()
USER_AGENT = "blaze resolve.py (https://github.com/indexable-inc/blaze)"
PARTS = ["bench", "crates", "workspaces"]
BENCH_PROJECT = re.compile(r'^    "([\w.-]+)": dict\(\s*repo="([^"]+)",[^\n]*?rev="([0-9a-f]{40})"', re.M)
CLASSES = ["equal", "different", "blaze-failed", "cargo-failed", "both-failed"]


def option(argv, name, default=None):
    if name in argv:
        i = argv.index(name)
        value = argv[i + 1]
        del argv[i : i + 2]
        return value
    return default


def run(command, cwd, env):
    proc = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True, errors="replace")
    return proc.returncode, proc.stdout, proc.stderr


def normalize(text, root, cargo_home):
    """The output with the machine's paths replaced, parsed and printed again in its own order."""
    for path, name in ((os.path.realpath(root), "$ROOT"), (root, "$ROOT"), (cargo_home, "$CARGO_HOME")):
        text = text.replace(json.dumps(path)[1:-1], name).replace("file://" + path, "file://" + name)
    return json.dumps(json.loads(text), indent=1)


def first_difference(a, b, at="$"):
    if type(a) is not type(b):
        return f"{at}: {json.dumps(a)[:300]} != {json.dumps(b)[:300]}"
    if isinstance(a, dict):
        if list(a) != list(b):
            missing = [k for k in b if k not in a]
            extra = [k for k in a if k not in b]
            if missing or extra:
                return f"{at}: blaze lacks {missing}, has extra {extra}"
            return f"{at}: keys in order {list(a)}, cargo's {list(b)}"
        for k in a:
            if (d := first_difference(a[k], b[k], f"{at}.{k}")) is not None:
                return d
        return None
    if isinstance(a, list):
        for i, (x, y) in enumerate(zip(a, b)):
            if (d := first_difference(x, y, f"{at}[{i}]")) is not None:
                return d
        if len(a) != len(b):
            return f"{at}: {len(a)} items, cargo {len(b)}"
        return None
    return None if a == b else f"{at}: {json.dumps(a)[:300]} != {json.dumps(b)[:300]}"


def first_line(stderr):
    lines = [l for l in stderr.splitlines() if l.startswith("error")]
    return (lines[0] if lines else stderr.strip().splitlines()[-1] if stderr.strip() else "")[:300]


def error_text(stderr):
    """The error cargo printed, from its first `error:` line on, with blank lines dropped."""
    lines = stderr.splitlines()
    start = next((i for i, l in enumerate(lines) if l.startswith("error")), len(lines))
    return "\n".join(l.rstrip() for l in lines[start:] if l.strip())


def clean_env(cargo_home):
    env = {k: v for k, v in os.environ.items() if not k.startswith("CARGO_") or k == "CARGO_ORACLE"}
    return {**env, "CARGO_HOME": cargo_home}


def compare(case, blaze, cargo_home):
    """Runs both on the workspace at case["dir"] and classifies the result."""
    root = case["dir"]
    env = clean_env(cargo_home)
    args = ["metadata", "--no-deps", "--format-version", "1", "--offline"]
    c_code, c_out, c_err = run(CARGO + args, root, env)
    b_code, b_out, b_err = run([blaze, *args[1:]], root, env)
    result = dict(name=case["name"], part=case["part"])
    if c_code and b_code:
        c_error, b_error = error_text(c_err), error_text(b_err)
        result.update(cls="both-failed", same_error=c_error == b_error)
        if c_error != b_error:
            result.update(cargo=c_error[:600], blaze=b_error[:600])
    elif c_code:
        result.update(cls="cargo-failed", cargo=first_line(c_err))
    elif b_code:
        result.update(cls="blaze-failed", blaze=first_line(b_err))
    else:
        c_json, b_json = normalize(c_out, root, cargo_home), normalize(b_out, root, cargo_home)
        if c_json == b_json:
            result.update(cls="equal")
        else:
            result.update(cls="different", difference=first_difference(json.loads(b_json), json.loads(c_json)) or "formatting")
    return result


def fetch_git(repo, rev, dest):
    if os.path.exists(os.path.join(dest, ".fetched")):
        return
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest)
    git = lambda *a: subprocess.run(["git", "-C", dest, *a], check=True, capture_output=True)
    git("init", "-q")
    git("fetch", "-q", "--depth", "1", f"https://github.com/{repo}", rev)
    git("-c", "advice.detachedHead=false", "checkout", "-q", "FETCH_HEAD")
    open(os.path.join(dest, ".fetched"), "w").close()


def fetch_crate(name, version, dest):
    if os.path.exists(os.path.join(dest, ".fetched")):
        return
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest)
    url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request) as response, tempfile.TemporaryFile() as file:
        shutil.copyfileobj(response, file)
        file.seek(0)
        with tarfile.open(fileobj=file, mode="r:gz") as archive:
            archive.extractall(dest, filter="data")
    open(os.path.join(dest, ".fetched"), "w").close()


def cases_of(part, corpus, work, limit):
    """The workspaces of a part, fetched: (name, directory) each."""
    cases = []
    if part == "bench":
        # Read, not imported: bench.py imports what needs a built ra-jit.
        source = open(os.path.join(HERE, "bench.py")).read()
        entries = [dict(name=n, repo=r, rev=v) for n, r, v in BENCH_PROJECT.findall(source)]
    else:
        entries = corpus[part]
    for entry in entries[:limit]:
        if part == "crates":
            name, version = entry.split("@")
            dest = os.path.join(work, "crates", entry)
            fetch = lambda d=dest, n=name, v=version: fetch_crate(n, v, d)
            directory = os.path.join(dest, f"{name}-{version}")
            cases.append(dict(name=entry, part=part, dir=directory, fetch=fetch))
        else:
            dest = os.path.join(work, part, entry["name"])
            fetch = lambda d=dest, e=entry: fetch_git(e["repo"], e["rev"], d)
            cases.append(dict(name=entry["name"], part=part, dir=os.path.join(dest, entry.get("path", "")), fetch=fetch))
    return cases


def build_blaze():
    command = CARGO + ["build", "--release", "-p", "blaze-manifest", "--example", "metadata", "--message-format", "json"]
    proc = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    for line in proc.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message["target"]["name"] == "metadata":
            return message["executable"]
    raise SystemExit("cargo built no metadata example")


def write_fixtures():
    cargo_home = tempfile.mkdtemp(prefix="blaze-fixture-home-")
    for name in sorted(os.listdir(FIXTURES)):
        source = os.path.join(FIXTURES, name)
        work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-fixture-"))
        root = os.path.join(work, name)
        shutil.copytree(source, root, ignore=shutil.ignore_patterns("expected.json", "fixture.json"))
        manifest = json.load(open(os.path.join(source, "fixture.json"))) if os.path.exists(os.path.join(source, "fixture.json")) else {}
        cwd = os.path.join(root, manifest.get("cwd", ""))
        env = clean_env(cargo_home)
        code, out, err = run(CARGO + ["metadata", "--no-deps", "--format-version", "1"], cwd, env)
        if code:
            error = error_text(err).replace(os.path.realpath(root), "$ROOT").replace(root, "$ROOT")
            text = json.dumps({"error": error}, indent=1)
        else:
            text = normalize(out, root, cargo_home)
        with open(os.path.join(source, "expected.json"), "w") as f:
            f.write(text + "\n")
        print(f"{name}: {'error' if code else 'ok'}")
        shutil.rmtree(work)
    shutil.rmtree(cargo_home)


def top_crates(count):
    names = []
    page = 1
    while len(names) < count:
        url = f"https://crates.io/api/v1/crates?sort=downloads&per_page=100&page={page}"
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        crates = json.load(urllib.request.urlopen(request))["crates"]
        if not crates:
            break
        names += [f"{c['id']}@{c['max_stable_version'] or c['newest_version']}" for c in crates]
        page += 1
    corpus = json.load(open(CORPUS))
    corpus["crates"] = names[:count]
    with open(CORPUS, "w") as f:
        json.dump(corpus, f, indent=1)
        f.write("\n")
    print(f"{len(corpus['crates'])} crates in {CORPUS}")


def main():
    argv = sys.argv[1:]
    if "--fixtures" in argv:
        return write_fixtures()
    if (count := option(argv, "--top-crates")) is not None:
        return top_crates(int(count))
    work = os.path.abspath(option(argv, "--work", os.path.join(tempfile.gettempdir(), "blaze-resolve")))
    jobs = int(option(argv, "--jobs", str(os.cpu_count() or 4)))
    out = option(argv, "--out")
    limit = int(option(argv, "--limit", "1000000"))
    blaze = option(argv, "--blaze") or build_blaze()
    cargo_home = os.path.join(work, "cargo-home")
    os.makedirs(cargo_home, exist_ok=True)

    dirs = [a for a in argv if a not in PARTS]
    if dirs:
        cases = [dict(name=d, part="dirs", dir=os.path.abspath(d), fetch=lambda: None) for d in dirs]
    else:
        corpus = json.load(open(CORPUS))
        cases = [c for part in (argv or PARTS) for c in cases_of(part, corpus, work, limit)]

    def one(case):
        try:
            case["fetch"]()
        except Exception as e:
            return dict(name=case["name"], part=case["part"], cls="fetch-failed", error=str(e)[:300])
        return compare(case, blaze, cargo_home)

    results = []
    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        for result in pool.map(one, cases):
            results.append(result)
            if result["cls"] != "equal" and not result.get("same_error"):
                detail = result.get("difference") or "\n  ".join(f"{k}: {result[k]}".replace("\n", "\n    ") for k in ("blaze", "cargo", "error") if k in result)
                print(f"{result['part']}/{result['name']}: {result['cls']}: {detail}", flush=True)

    print()
    for part in dict.fromkeys(r["part"] for r in results):
        rows = [r for r in results if r["part"] == part]
        counts = {c: sum(r["cls"] == c for r in rows) for c in CLASSES + ["fetch-failed"]}
        same = sum(r.get("same_error", False) for r in rows)
        others = [f"{v} {k}" + (f" ({same} with cargo's error text)" if k == "both-failed" else "") for k, v in counts.items() if v and k != "equal"]
        print(f"{part}: {counts['equal']}/{len(rows)} equal" + "".join(", " + o for o in others))
    if out:
        with open(out, "w") as f:
            json.dump(results, f, indent=1)
            f.write("\n")
    failed = ("different", "blaze-failed", "cargo-failed")
    return 1 if any(r["cls"] in failed or r.get("same_error") is False for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
