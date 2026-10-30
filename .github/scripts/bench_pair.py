#!/usr/bin/env python3
"""Prepare the verified bundle's benchmark pair; never compile either side."""
import hashlib
import json
import os
from pathlib import Path
import shutil


def executable(bundle, name):
    entry = bundle["sets"][name]
    bins = [item for item in entry["executables"] if item["name"] == "store" and "bench" in item["kind"]]
    assert len(bins) == 1, f"{name}: expected exactly one store benchmark"
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
    return path


def prepare(bundle, base_sha, temp):
    head = bundle["sets"]["bench-head"]
    assert head["kind"] == "bench" and head["source"] == "head"
    expected_args = ["-p", "ra-jit-bench", "--bench", "store"]
    assert head["args"] == expected_args, "head benchmark build configuration changed"
    temp.mkdir(parents=True, exist_ok=True)
    for name in ("bench-base", "bench-head"):
        (temp / name).unlink(missing_ok=True)
    # A missing pair is an error. Only an explicit producer record that the base
    # has no benchmark package preserves the old solo-benchmark exception.
    base = bundle["sets"].get("bench-base")
    if base_sha:
        assert base is not None, "producer omitted the PR base benchmark"
        assert base["source"] == "base", "producer used head source for both sides"
        assert base["identity"]["commit"] == base_sha, "benchmark base is not the PR base"
        assert base["identity"]["tree"], "benchmark base tree was not recorded"
        assert base["args"] == head["args"], "benchmark pair uses different flags/profiles"
        if base.get("absent"):
            assert base["reason"] == "crates/ra-jit-bench does not exist in base", base
            assert not base.get("executables"), "absent benchmark has an executable"
        else:
            assert base["kind"] == head["kind"], "benchmark pair uses different Cargo modes"
            shutil.copy2(executable(bundle, "bench-base"), temp / "bench-base")
    else:
        assert base is None or (base.get("absent") and base.get("reason") == "no base commit (not a pull request)" and not base.get("executables")), "non-PR run unexpectedly contains a base benchmark"
    shutil.copy2(executable(bundle, "bench-head"), temp / "bench-head")
    evidence = temp / "tango"
    evidence.mkdir(exist_ok=True)
    (evidence / "bundle.json").write_text(json.dumps(bundle, indent=2) + "\n")
    print("Prepared base/head pair" if (temp / "bench-base").exists() else "Prepared head-only benchmark")


if __name__ == "__main__":
    prepare(json.loads(Path(os.environ["CI_BUNDLE_JSON"]).read_text()),
            os.environ.get("BASE_SHA", ""), Path(os.environ["RUNNER_TEMP"]))
