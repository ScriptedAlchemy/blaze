#!/usr/bin/env python3
"""Prepare the verified bundle's benchmark pairs; never compile either side.

Each benchmark is a pair of sets of .github/ci/test-sets.json, `bench-<name>-head` and
`bench-<name>-base`, one cargo command each (`-p <package> --bench <name>`). For each pair this
checks that the bundle built both as test-sets.json says, the base from the pull request's base
commit with the head's arguments and flags, and that each executable is the one bundle.json pins;
then it copies them to $RUNNER_TEMP/bench/<name>-head and <name>-base and says what the job is to
do with the pair, in $RUNNER_TEMP/bench/pairs.txt (`<name> <state>` per line) and, with both
digests, in $RUNNER_TEMP/tango/pairs.json:

  identical  the two executables have the same sha256: the change left everything the benchmark
             links alone, and it is not judged (bench.yaml's header says why it is not measured);
  differs    they differ: the A/A control and the comparisons judge it (bench_verdict.py);
  head-only  no base commit (a push), or a base that has no such benchmark package, which only an
             explicit record of the producer's says (a base from before the split, whose
             crates/ra-jit-bench has a bench `store` too, is one): the head is run alone.

A base the producer did not record is an error, never head-only.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil

NO_BASE = "no base commit (not a pull request)"
PLATFORM = "linux-x64"


def pairs(spec):
    """{bench: (head set name, base set name)} of test-sets.json, for this platform."""
    found = {}
    for name, entry in spec.items():
        match = re.fullmatch(r"bench-(.+)-head", name)
        if match and PLATFORM in entry["platforms"]:
            found[match.group(1)] = (name, f"bench-{match.group(1)}-base")
    assert found, "test-sets.json has no benchmark pair"
    for bench, (_, base) in found.items():
        assert base in spec, f"{bench}: test-sets.json has a head and no base"
    return found


def executable(bundle, name, bench):
    entry = bundle["sets"][name]
    bins = [item for item in entry["executables"] if item["name"] == bench and "bench" in item["kind"]]
    assert len(bins) == 1, f"{name}: expected exactly one {bench} benchmark"
    path = Path(bins[0]["path"])
    relative = None
    for root, location in bundle["roots"].items():
        try:
            relative = root + "/" + path.relative_to(location).as_posix()
            break
        except ValueError:
            pass
    assert relative is not None, f"{name}: executable outside bundle roots"
    pin = bundle["files"][relative]
    assert path.is_file() and os.access(path, os.X_OK), f"{name}: not executable"
    assert pin["executable"] and path.stat().st_size == pin["size"], name
    assert hashlib.sha256(path.read_bytes()).hexdigest() == pin["sha256"], name
    return path, pin["sha256"]


def same_build(entry, want, name):
    """The bundle's entry was built as test-sets.json says."""
    assert entry["kind"] == "bench", f"{name}: not a bench set"
    assert entry["args"] == want["bench"], f"{name}: benchmark build configuration changed"
    for field in ("rustflags", "remap-paths"):
        assert entry.get(field) == want.get(field), f"{name}: built with other {field}"


def prepare_pair(bundle, spec, bench, head_name, base_name, base_sha, out):
    head, base = bundle["sets"].get(head_name), bundle["sets"].get(base_name)
    assert head is not None, f"producer omitted {head_name}"
    assert head["source"] == "head", f"{head_name}: not built from the head"
    same_build(head, spec[head_name], head_name)
    want = spec[base_name]
    row = {"bench": bench, "base": None}
    if base_sha:
        assert base is not None, f"producer omitted the PR base benchmark {base_name}"
        assert base["source"] == "base", f"{base_name}: producer used head source for both sides"
        assert base["identity"]["commit"] == base_sha, f"{base_name}: benchmark base is not the PR base"
        assert base["identity"]["tree"], f"{base_name}: benchmark base tree was not recorded"
        assert base["args"] == head["args"], f"{bench}: benchmark pair uses different flags/profiles"
        for field in ("rustflags", "remap-paths"):
            assert base.get(field) == head.get(field), f"{bench}: benchmark pair uses different flags/profiles"
        if base.get("absent"):
            requires = want["requires"]
            assert base.get("requires") == requires, f"{base_name}: absent for another requirement"
            reason = f"the base has no package {requires['package']} with bench target {requires['bench']}"
            assert base["reason"] == reason, base
            assert not base.get("executables"), f"{base_name}: absent benchmark has an executable"
            row["reason"] = reason
        else:
            same_build(base, want, base_name)
            path, digest = executable(bundle, base_name, bench)
            shutil.copy2(path, out / f"{bench}-base")
            row["base"] = digest
    else:
        assert base is None or (base.get("absent") and base.get("reason") == NO_BASE
                                and not base.get("executables")), \
            f"{base_name}: non-PR run unexpectedly contains a base benchmark"
        row["reason"] = NO_BASE
    path, row["head"] = executable(bundle, head_name, bench)
    shutil.copy2(path, out / f"{bench}-head")
    if row["base"] is None:
        row["state"] = "head-only"
    else:
        row["state"] = "identical" if row["base"] == row["head"] else "differs"
    return row


def prepare(bundle, spec, base_sha, temp):
    out = temp / "bench"
    # Nothing of an earlier pair may be taken for this one's.
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)
    rows = [prepare_pair(bundle, spec, bench, head, base, base_sha, out)
            for bench, (head, base) in sorted(pairs(spec).items())]
    evidence = temp / "tango"
    evidence.mkdir(exist_ok=True)
    (evidence / "bundle.json").write_text(json.dumps(bundle, indent=2) + "\n")
    (evidence / "pairs.json").write_text(json.dumps(rows, indent=2) + "\n")
    (out / "pairs.txt").write_text("".join(f"{row['bench']} {row['state']}\n" for row in rows))
    for row in rows:
        print(f"{row['bench']}: {row['state']}" + (f" ({row['reason']})" if row.get("reason") else ""))
    return rows


if __name__ == "__main__":
    prepare(json.loads(Path(os.environ["CI_BUNDLE_JSON"]).read_text()),
            json.loads(Path(os.environ.get("CI_TEST_SETS", ".github/ci/test-sets.json")).read_text()),
            os.environ.get("BASE_SHA", ""), Path(os.environ["RUNNER_TEMP"]))
