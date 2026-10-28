#!/usr/bin/env python3
"""compare.py: Bazel's test results against blaze's, over the inventory of unit S2.

    python3 -I harness/dist/ci/compare.py BAZEL_TEST_LOGS BLAZE_EVENTS --inventory INVENTORY
                                      compare one run of each side; exit 1 on any difference
    python3 -I harness/dist/ci/compare.py --self-test
                                      a synthetic run per violation, each of which has to fail

The inventory is `harness/dist/ci/inventory.py`'s file for the commit both sides ran (unit S2,
docs/distributed.md section 10). The comparison is against the inventory, never "the tests both
ran": a test missing on either side, extra on either side, not executed on either side, a test the
inventory keeps on Bazel that Bazel did not execute or that blaze did execute, a skip without the
validated manifest its cached pass needs (section 6), and a different verdict all fail. Exit 0
means the run agrees with the inventory; the counts are printed either way.

The inventory keeps some tests on Bazel by name (`keeps_on_bazel`, the `rustc-tests` gate's
default), because they spawn rustc or cargo. Each of those must be executed and pass on Bazel and
must not appear in blaze's events; the rest of its target is compared as usual. A target the
inventory classes `bazel` is kept whole. A target it classes `doctest` is blaze's alone, since
Bazel has no rule that runs a doctest. A `manual` target is in neither side's comparison, and
running it is a note.

**BAZEL_TEST_LOGS** is `bazel-testlogs/` as CI uploads it: a directory of `<package>/<target>/`
with the test's `test.log` and `test.xml` in it. The label is the path below the directory,
`bazel-testlogs/crates/x/mcp_suite/test.xml`; a sharded result (`.../shard_1_of_2/test.xml`)
belongs to the target above the shard directory. `test.log` is read first, because libtest names
every test it ran there (`test mod::name ... ok`), which is what the per-test comparison needs; the
XML is the fallback, and one XML case is the target's own result whatever it is called, which is
what Bazel synthesizes for a test that writes no XML of its own. One `test.xml` on its own is
refused: the directory it sits in names the target, and a path alone does not name the package.

**BLAZE_EVENTS** is the job's event stream as a person reads it (JSON is only a rendering of the
wire's CBOR, docs/roadmap.md section 8): one JSON object per line, each a `TestVerdict` with the
target it belongs to. Two spellings are read, and nothing else:

    {"sequence": 7, "attempt": 0, "event": {"Verdict": {"target": "//crates/x:mcp_suite",
                                                        "test": "mcp_handler_test::a",
                                                        "passed": true, "skipped": false,
                                                        "manifest": "…", "manifest_held": null,
                                                        "duration_ns": 12}}}
    {"kind": "verdict", "target": "//crates/x:mcp_suite", "test": "…", "passed": true, …}

`skipped` says the test did not run because a pass was found for it (section 6, step 4). Such a
record is accepted only with `manifest_held: true`: the pass's manifest was checked against this
machine's bundle and files and held. A skip without it is the same failure as a test that did not
execute. `test` is the test's name inside the target, and is compared when both sides name tests.

Python 3 standard library only.
"""
import argparse
import json
import os
import re
import sys
import tempfile
import xml.etree.ElementTree as ET

FAIL, NOTE = "FAIL ", "note "


class Bad(Exception):
    pass


# --------------------------------------------------------------------------------------------
# The two sides


def label_of_path(root, path):
    """The target a `bazel-testlogs` path belongs to."""
    rel = os.path.relpath(path, root)
    parts = [p for p in rel.split(os.sep) if p]
    if parts and parts[-1] == "test.xml":
        parts = parts[:-1]
    if parts and parts[-1].startswith("shard_"):
        parts = parts[:-1]
    if not parts:
        raise Bad("%s names no target: expected <package>/<target>/test.xml" % path)
    name = parts[-1]
    package = "/".join(parts[:-1])
    return "//%s:%s" % (package, name) if package else "//:%s" % name


def read_bazel(root):
    """Bazel's results: label -> the cases of each of its shards, from `test.log` and `test.xml`."""
    if not os.path.exists(root):
        raise Bad("%s is not there: pass the `bazel-testlogs` directory CI uploads" % root)
    if not os.path.isdir(root):
        raise Bad("%s is not the `bazel-testlogs` directory: a single test.xml cannot name its "
                  "package, and the directory is what CI uploads" % root)
    found = {}
    for base, _, names in os.walk(root):
        for name in names:
            if name in ("test.xml", "test.log"):
                found.setdefault(os.path.dirname(os.path.join(base, name)), {})[name] = \
                    os.path.join(base, name)
    if not found:
        raise Bad("%s holds no test.xml: was the run's `bazel-testlogs` uploaded?" % root)
    out = {}
    for where in sorted(found):
        label = label_of_path(root, os.path.join(where, "test.xml"))
        # `test.log` is the test's own output, where libtest names every test it ran; the XML is
        # Bazel's summary, with a duration and a status for the file. The log wins on names.
        cases = parse_test_log(found[where]["test.log"]) if "test.log" in found[where] else []
        if not cases and "test.xml" in found[where]:
            cases = parse_test_xml(found[where]["test.xml"])
        if cases and "test.xml" in found[where]:
            # The XML says whether the whole file failed (a crash before libtest printed, a
            # timeout), which the log's per-test lines cannot show.
            xml = parse_test_xml(found[where]["test.xml"])
            if xml and all(not c["passed"] and not c["skipped"] for c in xml):
                for case in cases:
                    case["passed"] = False
                    case["why"] = xml[0]["why"] or "the test's XML reports a failure"
        if not cases:
            cases = [{"name": None, "passed": False, "skipped": False, "duration_s": 0.0,
                      "why": "neither test.log nor test.xml says what the test did"}]
        out.setdefault(label, []).extend(cases)
    return out


LIBTEST_LINE = re.compile(r"^test (?P<name>.+?) \.\.\. (?P<what>ok|FAILED|ignored|bench:.*)$")


def parse_test_log(path):
    """The tests libtest reported in a test's log, one case each, in the order it printed them.

    A name is taken once: a retried test prints again, and a suite that reruns one test in a child
    process (tracedecay's `rerun_test_in_child`) prints the same name a second time.
    """
    try:
        with open(path, errors="replace") as f:
            text = f.read()
    except OSError as error:
        raise Bad("%s cannot be read: %s" % (path, error))
    cases, seen = [], set()
    for line in text.split("\n"):
        m = LIBTEST_LINE.match(line.strip())
        if not m:
            continue
        name = m.group("name")
        if name in seen:
            continue  # a retried test prints again; the last line is the one that counts
        seen.add(name)
        what = m.group("what")
        cases.append({
            "name": name,
            "passed": what == "ok",
            "skipped": what == "ignored",
            "duration_s": 0.0,
            "why": "" if what == "ok" else what,
        })
    return cases


def parse_test_xml(path):
    try:
        root = ET.parse(path).getroot()
    except ET.ParseError as error:
        raise Bad("%s is not XML: %s" % (path, error))
    cases = []
    for case in root.iter("testcase"):
        failed = any(child.tag in ("failure", "error") for child in case)
        skipped = any(child.tag == "skipped" for child in case)
        why = ""
        for child in case:
            if child.tag in ("failure", "error", "skipped"):
                why = (child.get("message") or child.text or "").strip().split("\n")[0]
                break
        cases.append({
            "name": case.get("name"),
            "passed": not failed and not skipped,
            "skipped": skipped,
            "duration_s": float(case.get("time") or 0.0),
            "why": why,
        })
    return cases


def read_blaze(path):
    """Blaze's results: label -> the `TestVerdict` records the event stream carries."""
    if not os.path.isfile(path):
        raise Bad("%s is not a file: pass the run's event stream (one JSON object per line)"
                  % path)
    out = {}
    with open(path) as f:
        for number, line in enumerate(f, 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            try:
                record = json.loads(line)
            except ValueError as error:
                raise Bad("%s line %d is not JSON: %s" % (path, number, error))
            for verdict in verdicts_of(record):
                label = verdict.get("target") or verdict.get("label")
                if not label:
                    raise Bad("%s line %d is a verdict with no target: %s"
                              % (path, number, json.dumps(verdict, sort_keys=True)))
                out.setdefault(normalize_label(label), []).append({
                    "name": verdict.get("test") or verdict.get("name"),
                    "passed": bool(verdict.get("passed")),
                    "skipped": bool(verdict.get("skipped")),
                    "manifest_held": verdict.get("manifest_held"),
                    "manifest": verdict.get("manifest"),
                    "duration_s": float(verdict.get("duration_ns") or 0) / 1e9,
                    "why": verdict.get("why") or "",
                })
    return out


def verdicts_of(record):
    """Every `TestVerdict` a line of the event stream carries, in either spelling."""
    if not isinstance(record, dict):
        raise Bad("an event line is not a JSON object: %r" % (record,))
    if isinstance(record.get("event"), dict):
        event = record["event"]
        if len(event) == 1:
            kind, payload = next(iter(event.items()))
            if kind == "Verdict" and isinstance(payload, dict):
                return [payload]
            if kind in ("Skipped", "Skip") and isinstance(payload, dict):
                payload = dict(payload)
                payload["skipped"] = True
                return [payload]
        return []
    kind = record.get("kind") or record.get("type")
    if kind in ("verdict", "Verdict"):
        return [record]
    if kind in ("skip", "skipped", "Skipped"):
        record = dict(record)
        record["skipped"] = True
        return [record]
    return []


def normalize_label(label):
    label = label.strip()
    if label.startswith("//"):
        return label
    return "//" + label


# --------------------------------------------------------------------------------------------
# The comparison


def target_verdict(cases):
    """pass, fail, skip or missing for one target, from its cases."""
    if not cases:
        return "missing"
    ran = [c for c in cases if not c["skipped"]]
    if any(not c["passed"] for c in ran):
        return "fail"
    if ran:
        return "pass"
    return "skip"


def skip_is_valid(cases):
    return all(c.get("manifest_held") is True for c in cases if c["skipped"])


def compare(inventory, bazel, blaze):
    """Every difference from the inventory, and what agreed. Returns (failures, notes).

    A test the inventory keeps on Bazel (`keeps_on_bazel`, the `rustc-tests` gate's default) must
    be executed and pass on Bazel and must not run on blaze. Every other inventoried test must run
    on both sides, or be validly skipped on blaze, with the same verdict. Doctests are blaze's
    alone, since Bazel has no rule that runs one. `manual` targets are out of the comparison.
    """
    failures, notes = [], []
    tests = [r for r in inventory if r.get("record") == "test" and r["class"] != "manual"]
    known = {t["label"] for t in tests}
    manual = {r["label"] for r in inventory if r.get("record") == "test" and r["class"] == "manual"}
    for label in sorted(set(bazel) | set(blaze)):
        if label in known:
            continue
        if label in manual:
            notes.append("%s: manual in the inventory, so neither side has to run it; %s ran it"
                         % (label, " and ".join(s for s, got in (("Bazel", bazel),
                                                                 ("blaze", blaze))
                                                if label in got)))
            continue
        side = " and ".join(s for s, got in (("Bazel", bazel), ("blaze", blaze)) if label in got)
        failures.append("%s is not in the inventory, but %s reported it" % (label, side))
    for test in tests:
        label, klass = test["label"], test["class"]
        keeps = set(test.get("keeps_on_bazel") or [])
        here_b, here_z = bazel.get(label, []), blaze.get(label, [])
        if klass == "doctest":
            if here_b:
                failures.append("%s: Bazel ran a doctest, which the inventory says it does not"
                                % label)
            vz = target_verdict(here_z)
            if vz == "missing":
                failures.append("%s: blaze did not execute the crate's doctests" % label)
            elif vz == "fail":
                failures.append("%s: blaze failed the crate's doctests" % label)
            elif vz == "skip" and not skip_is_valid(here_z):
                failures.append("%s: blaze skipped the doctests without a validated manifest"
                                % label)
            elif vz == "pass":
                notes.append("%s: doctests passed on blaze, which Bazel does not run" % label)
            continue
        if klass == "bazel":
            if here_z:
                failures.append("%s: the inventory keeps it on Bazel (rustc-tests), and blaze "
                                "ran it" % label)
            elif not here_b:
                failures.append("%s: the inventory keeps it on Bazel (rustc-tests), and Bazel "
                                "did not execute it" % label)
            elif any(not c["passed"] and not c["skipped"] for c in here_b):
                failures.append("%s: the inventory keeps it on Bazel (rustc-tests), and it "
                                "failed there" % label)
            else:
                notes.append("%s: kept on Bazel, passed there, absent from blaze" % label)
            continue
        if keeps:
            failures.extend(kept_differences(label, keeps, here_b, here_z))
        # The tests of this target that blaze runs: every test Bazel reported, minus the ones
        # kept on Bazel. A side that reports the target as one case (Bazel's synthesized XML for
        # a test that writes none) cannot be split, and is compared whole.
        run_b = [c for c in here_b if not c["name"] or c["name"] not in keeps]
        run_z = [c for c in here_z if not c["name"] or c["name"] not in keeps]
        vb, vz = target_verdict(run_b), target_verdict(run_z)
        if not here_b:
            failures.append("%s: Bazel did not execute it, and the inventory has tests of it "
                            "for blaze's side to be compared with" % label)
            continue
        if not run_b and keeps and len(here_b) == len(keeps):
            continue  # every test of this target is kept on Bazel, and all of them ran there
        if vb == "missing":
            failures.append("%s: Bazel reported no test of it that blaze is to run" % label)
        elif vb == "skip":
            failures.append("%s: Bazel skipped it; only a cached pass with a validated manifest "
                            "is a skip, and Bazel's cache is not that" % label)
        if vz == "missing":
            failures.append("%s: blaze did not execute it and did not validly skip it" % label)
        elif vz == "skip" and not skip_is_valid(run_z):
            failures.append("%s: blaze skipped it without the cached pass's manifest holding on "
                            "this machine" % label)
        elif vb == "pass" and vz == "fail":
            failures.append("%s: Bazel passed it and blaze failed it" % label)
        elif vb == "fail" and vz == "pass":
            failures.append("%s: Bazel failed it and blaze passed it" % label)
        elif vb == "pass" and vz == "pass":
            notes.append("%s: passed on both sides" % label)
        if vb != "missing" and vz != "missing":
            failures.extend(per_test_differences(label, run_b, run_z, keeps))
        failures.extend(over_timeout(label, test, run_z))
    return failures, notes


def over_timeout(label, test, blaze_cases):
    """A test blaze ran past the timeout Bazel runs it under: the job did not honour it."""
    limit = test.get("timeout_s")
    if not limit:
        return []
    out = []
    for case in blaze_cases:
        took = case.get("duration_s") or 0.0
        if took > limit:
            out.append("%s: %s took %.1f s on blaze, past the %d s Bazel runs it under"
                       % (label, case["name"] or "the target", took, limit))
    return out


def kept_differences(label, keeps, bazel_cases, blaze_cases):
    """The tests kept on Bazel: executed and passing there, and absent from blaze."""
    out = []
    named_b = {c["name"]: c for c in bazel_cases if c["name"]}
    named_z = {c["name"]: c for c in blaze_cases if c["name"]}
    if not named_b and len(bazel_cases) == 1:
        return ["%s: keeps %d test%s on Bazel (%s), and Bazel's XML names no test, so which ones "
                "it ran cannot be checked"
                % (label, len(keeps), "" if len(keeps) == 1 else "s", ", ".join(sorted(keeps)))]
    for name in sorted(keeps):
        case = named_b.get(name)
        if case is None:
            out.append("%s: %s is kept on Bazel (rustc-tests) and Bazel did not execute it"
                       % (label, name))
        elif not case["passed"] and not case["skipped"]:
            out.append("%s: %s is kept on Bazel (rustc-tests) and failed there"
                       % (label, name))
        if name in named_z:
            out.append("%s: %s is kept on Bazel (rustc-tests) and blaze ran it"
                       % (label, name))
    return out


def per_test_differences(label, bazel_cases, blaze_cases, keeps=()):
    """Differences between the test names each side reported, when both name tests."""
    named_b = {c["name"] for c in bazel_cases if c["name"]} - set(keeps)
    named_z = {c["name"] for c in blaze_cases if c["name"]} - set(keeps)
    if len(bazel_cases) < 2 or len(blaze_cases) < 2:
        return []  # one case a side: the target's own result, not a test name
    out = []
    for name in sorted(named_z - named_b):
        out.append("%s: %s ran on blaze and Bazel did not report it" % (label, name))
    for name in sorted(named_b - named_z):
        out.append("%s: %s ran on Bazel and blaze did not report it" % (label, name))
    for name in sorted(named_b & named_z):
        b = [c for c in bazel_cases if c["name"] == name]
        z = [c for c in blaze_cases if c["name"] == name]
        if target_verdict(b) != target_verdict(z) and "skip" not in (
                target_verdict(b), target_verdict(z)):
            out.append("%s: %s passed on one side and failed on the other"
                       % (label, name))
    return out


def summarize(inventory, bazel, blaze):
    """Per class, the targets in the inventory and how many of them each side reported."""
    tests = [r for r in inventory if r.get("record") == "test"]
    counts = {}
    for test in tests:
        klass = test["class"]
        counts.setdefault(klass, {"inventory": 0, "bazel": 0, "blaze": 0, "kept_on_bazel": 0})
        counts[klass]["inventory"] += 1
        counts[klass]["kept_on_bazel"] += len(test.get("keeps_on_bazel") or [])
        if test["label"] in bazel:
            counts[klass]["bazel"] += 1
        if test["label"] in blaze:
            counts[klass]["blaze"] += 1
    return counts


def report(inventory, bazel, blaze):
    failures, notes = compare(inventory, bazel, blaze)
    counts = summarize(inventory, bazel, blaze)
    lines = []
    for klass in sorted(counts):
        got = counts[klass]
        kept = (", %d tests kept on Bazel" % got["kept_on_bazel"]) if got["kept_on_bazel"] else ""
        lines.append("%-8s %3d targets in the inventory%s; %3d reported by Bazel, %3d by blaze"
                     % (klass, got["inventory"], kept, got["bazel"], got["blaze"]))
    for line in failures:
        lines.append(FAIL + line)
    for line in notes:
        lines.append(NOTE + line)
    return failures, notes, counts, lines


# --------------------------------------------------------------------------------------------
# Self-test: one synthetic run per violation, each of which has to fail


def synthetic():
    """An inventory and two clean runs that agree with it, as the self-test's base."""
    inventory = [
        {"record": "inventory", "format": 1, "counts": {}},
        {"record": "test", "label": "//p:suite", "class": "blaze", "kind": "integration"},
        {"record": "test", "label": "//p:unit", "class": "blaze", "kind": "unit",
         "timeout_s": 300},
        {"record": "test", "label": "//p:toolchain", "class": "bazel", "kind": "integration"},
        {"record": "test", "label": "//p:mixed", "class": "blaze", "kind": "integration",
         "keeps_on_bazel": ["kept_test"]},
        {"record": "test", "label": "//p:manual", "class": "manual", "kind": "integration"},
        {"record": "test", "label": "//p:doctest", "class": "doctest", "kind": "doctest"},
    ]
    bazel = {
        "//p:suite": [{"name": "//p:suite", "passed": True, "skipped": False,
                       "duration_s": 1.0, "why": ""}],
        "//p:unit": [{"name": "//p:unit", "passed": True, "skipped": False,
                      "duration_s": 1.0, "why": ""}],
        "//p:toolchain": [{"name": "//p:toolchain", "passed": True, "skipped": False,
                           "duration_s": 1.0, "why": ""}],
        "//p:mixed": [{"name": "kept_test", "passed": True, "skipped": False,
                       "duration_s": 1.0, "why": ""},
                      {"name": "run_test", "passed": True, "skipped": False,
                       "duration_s": 1.0, "why": ""}],
    }
    blaze = {
        "//p:suite": [{"name": "a", "passed": True, "skipped": False, "manifest_held": None,
                       "duration_s": 1.0, "why": ""}],
        "//p:unit": [{"name": "b", "passed": True, "skipped": False, "manifest_held": None,
                      "duration_s": 1.0, "why": ""}],
        "//p:doctest": [{"name": "c", "passed": True, "skipped": False, "manifest_held": None,
                         "duration_s": 1.0, "why": ""}],
        "//p:mixed": [{"name": "run_test", "passed": True, "skipped": False,
                       "manifest_held": None, "duration_s": 1.0, "why": ""}],
    }
    return inventory, bazel, blaze


def whole_inventory_selftest(inventory):
    """A synthetic agreeing run of every test in a real inventory, then one target dropped."""
    checks, failures = [], []

    def cases(names):
        return [{"name": name, "passed": True, "skipped": False, "duration_s": 0.01,
                 "manifest_held": None, "why": ""} for name in names]

    tests = [r for r in inventory if r.get("record") == "test" and r["class"] != "manual"]
    bazel, blaze = {}, {}
    for test in tests:
        label = test["label"]
        kept = list(test.get("keeps_on_bazel") or [])
        if test["class"] == "doctest":
            blaze[label] = cases(["a doctest"])
            continue
        bazel[label] = cases(["a test"] + kept)
        blaze[label] = cases(["a test"])
    checks.append("a whole inventory agrees")
    got, _ = compare(inventory, bazel, blaze)
    if got:
        failures.append("a whole inventory agrees: %d differences (%s)"
                        % (len(got), "; ".join(got[:3])))
    checks.append("a whole inventory with one target dropped fails")
    for klass in ("blaze", "doctest"):
        candidate = next((t for t in tests if t["class"] == klass), None)
        if candidate is None:
            continue
        dropped = {k: v for k, v in blaze.items() if k != candidate["label"]}
        got, _ = compare(inventory, bazel, dropped)
        if not any(candidate["label"] in g for g in got):
            failures.append("a whole inventory with one target dropped fails: %s was not named "
                            "in %r" % (candidate["label"], got[:3]))
    return checks, failures


def selftest(against=None):
    import copy

    failures = []
    checks = []

    def expect(name, want_fail, inventory, bazel, blaze):
        checks.append(name)
        got, notes = compare(inventory, bazel, blaze)
        if bool(got) != want_fail:
            failures.append("%s: %d differences (%s), wanted %s"
                            % (name, len(got), "; ".join(got) or "none",
                               "a failure" if want_fail else "none"))

    inventory, bazel, blaze = synthetic()
    expect("clean", False, inventory, bazel, blaze)
    base_b, base_z = copy.deepcopy(bazel), copy.deepcopy(blaze)

    def run(name, mutate):
        inv, bz, zl = copy.deepcopy(inventory), copy.deepcopy(base_b), copy.deepcopy(base_z)
        mutate(inv, bz, zl)
        expect(name, True, inv, bz, zl)

    run("blaze drops a test", lambda i, b, z: z.pop("//p:unit"))
    run("blaze adds a test", lambda i, b, z: z.update(
        {"//p:extra": [{"name": "x", "passed": True, "skipped": False, "manifest_held": None,
                        "duration_s": 1.0, "why": ""}]}))
    run("bazel drops a test", lambda i, b, z: b.pop("//p:unit"))
    run("bazel adds a test", lambda i, b, z: b.update(
        {"//p:extra": [{"name": "x", "passed": True, "skipped": False, "duration_s": 1.0,
                        "why": ""}]}))
    run("a different verdict", lambda i, b, z: z["//p:unit"][0].update({"passed": False}))
    run("bazel keeps a test on Bazel that blaze ran",
        lambda i, b, z: z.update({t["label"]: base_z["//p:unit"]
                                  for t in i if t.get("label") == "//p:toolchain"}))
    run("bazel does not execute a test kept on Bazel", lambda i, b, z: b.pop("//p:toolchain"))
    run("blaze runs a test kept on Bazel", lambda i, b, z: z["//p:mixed"].append(
        {"name": "kept_test", "passed": True, "skipped": False, "manifest_held": None,
         "duration_s": 1.0, "why": ""}))
    run("bazel does not run a test kept on Bazel", lambda i, b, z: b["//p:mixed"].pop(0))
    run("a test kept on Bazel fails there", lambda i, b, z: b["//p:mixed"][0].update(
        {"passed": False}))
    run("blaze does not run the rest of a partly kept target", lambda i, b, z: z.pop("//p:mixed"))
    run("blaze runs a test past Bazel's timeout", lambda i, b, z: z["//p:unit"][0].update(
        {"duration_s": 301.0}))
    run("a skip without a validated manifest", lambda i, b, z: z["//p:unit"][0].update(
        {"skipped": True, "passed": True, "manifest_held": False}))
    run("a skip with no manifest at all", lambda i, b, z: z["//p:unit"][0].update(
        {"skipped": True, "passed": True, "manifest_held": None}))
    run("blaze does not run a doctest", lambda i, b, z: z.pop("//p:doctest"))
    run("bazel runs a doctest", lambda i, b, z: b.update({"//p:doctest": base_z["//p:doctest"]}))
    # A manual target is not required on either side: running it is a note, not a difference.
    inv, bz, zl = copy.deepcopy(inventory), copy.deepcopy(base_b), copy.deepcopy(base_z)
    zl["//p:manual"] = [{"name": "m", "passed": False, "skipped": False, "manifest_held": None,
                         "duration_s": 1.0, "why": ""}]
    got, noted = compare(inv, bz, zl)
    checks.append("a manual target is a note")
    if got or not any("//p:manual" in n and "manual in the inventory" in n for n in noted):
        failures.append("a manual target is a note: %d differences, notes %r" % (len(got), noted))

    # A skip with the manifest held is accepted, which is the one way a test may not execute.
    inv, bz, zl = copy.deepcopy(inventory), copy.deepcopy(base_b), copy.deepcopy(base_z)
    zl["//p:unit"][0].update({"skipped": True, "manifest_held": True})
    expect("a validated skip is accepted", False, inv, bz, zl)

    # Per-test names are compared when both sides report them.
    inv, bz, zl = copy.deepcopy(inventory), copy.deepcopy(base_b), copy.deepcopy(base_z)
    bz["//p:suite"] = [{"name": "a", "passed": True, "skipped": False, "duration_s": 1.0,
                        "why": ""},
                       {"name": "gone", "passed": True, "skipped": False, "duration_s": 1.0,
                        "why": ""}]
    zl["//p:suite"] = [{"name": "a", "passed": True, "skipped": False, "manifest_held": None,
                        "duration_s": 1.0, "why": ""},
                       {"name": "new", "passed": True, "skipped": False, "manifest_held": None,
                        "duration_s": 1.0, "why": ""}]
    expect("a test name on one side only", True, inv, bz, zl)

    # The two sides are read from the files they are handed, in the shapes documented above.
    with tempfile.TemporaryDirectory() as work:
        logs = os.path.join(work, "bazel-testlogs")
        os.makedirs(os.path.join(logs, "p", "suite"))
        os.makedirs(os.path.join(logs, "p", "unit"))
        os.makedirs(os.path.join(logs, "p", "toolchain"))
        os.makedirs(os.path.join(logs, "p", "manual"))
        os.makedirs(os.path.join(logs, "p", "mixed"))
        for target, body in [
            ("suite", '<testcase name="//p:suite" time="1.0"/>'),
            ("unit", '<testcase name="b" time="1.0"/>'),
            ("toolchain", '<testcase name="//p:toolchain" time="1.0"/>'),
            ("manual", '<testcase name="//p:manual" time="1.0"><failure message="x"/></testcase>'),
            ("mixed", '<testcase name="kept_test" time="1.0"/>\n'
                      '<testcase name="run_test" time="1.0"/>'),
        ]:
            path = os.path.join(logs, "p", target, "test.xml")
            with open(path, "w") as f:
                f.write('<?xml version="1.0"?>\n<testsuite>\n' + body + "\n</testsuite>\n")
            if target == "mixed":
                with open(os.path.join(logs, "p", target, "test.log"), "w") as f:
                    f.write("running 2 tests\ntest kept_test ... ok\ntest run_test ... ok\n"
                            "test result: ok. 2 passed; 0 failed; 0 ignored\n")
        events = os.path.join(work, "events.jsonl")
        with open(events, "w") as f:
            f.write(json.dumps({"sequence": 1, "attempt": 0, "event": {"Verdict": {
                "target": "//p:suite", "test": "a", "passed": True, "skipped": False,
                "manifest": "00", "manifest_held": None, "duration_ns": 5}}}) + "\n")
            f.write(json.dumps({"kind": "verdict", "target": "//p:unit", "test": "b",
                                "passed": True, "duration_ns": 5}) + "\n")
            f.write(json.dumps({"kind": "verdict", "target": "//p:doctest", "test": "c",
                                "passed": True, "duration_ns": 5}) + "\n")
            f.write(json.dumps({"kind": "verdict", "target": "//p:mixed", "test": "run_test",
                                "passed": True, "duration_ns": 5}) + "\n")
        read_b = read_bazel(logs)
        read_z = read_blaze(events)
        checks.append("reads bazel logs")
        if sorted(read_b) != ["//p:manual", "//p:mixed", "//p:suite", "//p:toolchain",
                              "//p:unit"]:
            failures.append("reads bazel logs: got %r" % sorted(read_b))
        checks.append("reads the event stream")
        if sorted(read_z) != ["//p:doctest", "//p:mixed", "//p:suite", "//p:unit"]:
            failures.append("reads the event stream: got %r" % sorted(read_z))
        checks.append("reads both spellings")
        if not (read_z["//p:suite"][0]["name"] == "a" and read_z["//p:unit"][0]["name"] == "b"):
            failures.append("reads both spellings: %r" % read_z)
        expect("the files' run agrees", False, inventory, read_b, read_z)
        checks.append("a crash with no libtest lines is a failure")
        crash = os.path.join(logs, "p", "crash")
        os.makedirs(crash)
        with open(os.path.join(crash, "test.log"), "w") as f:
            f.write("exec ${PAGER:-/usr/bin/less} \"$0\" \"$@\"\n")
        with open(os.path.join(crash, "test.xml"), "w") as f:
            f.write('<testsuite><testcase name="//p:crash" time="0.1">'
                    '<failure message="timed out"/></testcase></testsuite>')
        read_crash = read_bazel(logs)
        if target_verdict(read_crash.get("//p:crash", [])) != "fail":
            failures.append("a crash with no libtest lines is a failure: got %r"
                            % (read_crash.get("//p:crash"),))
        checks.append("a sharded path names the target")
        try:
            got = label_of_path(logs, os.path.join(logs, "p", "suite", "shard_1_of_2",
                                                    "test.xml"))
        except Exception as error:  # noqa: BLE001 - the check is that it does not raise
            got = repr(error)
        if got != "//p:suite":
            failures.append("a sharded path names the target: got %r" % (got,))

    if against:
        real = read_inventory(against)
        more_checks, more_failures = whole_inventory_selftest(real)
        checks.extend(more_checks)
        failures.extend(more_failures)
    for failure in failures:
        print("compare.py --self-test: " + failure)
    print("compare.py --self-test: %d checks, %d failed" % (len(checks), len(failures)))
    return 1 if failures else 0


# --------------------------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("bazel", nargs="?", help="the `bazel-testlogs` directory of a run")
    ap.add_argument("blaze", nargs="?", help="the run's event stream, one JSON object per line")
    ap.add_argument("--inventory", help="the inventory file from harness/dist/ci/inventory.py")
    ap.add_argument("--json", help="write the comparison here, as JSON")
    ap.add_argument("--quiet", action="store_true", help="print the counts and failures only")
    ap.add_argument("--self-test", action="store_true", help="a synthetic run per violation")
    ap.add_argument("--against", metavar="INVENTORY",
                    help="with --self-test: also compare a synthetic agreeing run of every test "
                         "in this inventory, and one that drops a target, so the rules are proven "
                         "on the real list and not only on the small one")
    a = ap.parse_args()
    if a.self_test:
        return selftest(a.against)
    if not (a.bazel and a.blaze and a.inventory):
        ap.error("BAZEL_TEST_LOGS, BLAZE_EVENTS and --inventory are all required")
    try:
        from_inventory = read_inventory(a.inventory)
        bazel = read_bazel(a.bazel)
        blaze = read_blaze(a.blaze)
        failures, notes, counts, lines = report(from_inventory, bazel, blaze)
    except Bad as error:
        print("compare.py: " + str(error), file=sys.stderr)
        return 2
    for line in lines:
        if a.quiet and line.startswith(NOTE):
            continue
        print(line)
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"counts": counts, "failures": failures, "notes": notes}, f,
                      indent=2, sort_keys=True)
            f.write("\n")
    if failures:
        print("compare.py: %d differences from the inventory" % len(failures))
        return 1
    print("compare.py: 0 differences from the inventory")
    return 0


def read_inventory(path):
    records = []
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                records.append(json.loads(line))
    except (OSError, ValueError) as error:
        raise Bad("%s is not the inventory file: %s" % (path, error))
    if not records or records[0].get("record") != "inventory":
        raise Bad("%s does not start with an inventory record: run harness/dist/ci/inventory.py"
                  % path)
    if not records[0].get("approved"):
        print("compare.py: note: the inventory of %s is not approved yet (the owner sets "
              "`approved` to true)" % records[0].get("source", {}).get("commit", "?"))
    return records


if __name__ == "__main__":
    sys.exit(main())
