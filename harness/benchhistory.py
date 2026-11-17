#!/usr/bin/env python3
"""benchhistory.py: the benchmark history the site's /bench page charts, and the regression check.

    benchhistory.py '{"action":"build", "result":"RESULT.json", "suite":"build"}'
    benchhistory.py '{"action":"live", "files":["LATENCY.json"]}'
    benchhistory.py '{"action":"check", "history":"HISTORY.jsonl", "report":"report.md"}'

One strict frozen JSON object, inline, a file path or `-`. Build, Live and Check below define
every field. Schema 2 keeps only newly measured Blaze fields and their compiler identity;
Cargo fields in older history lines remain historical Cargo measurements, never new Blaze.

`build` appends one line to FILE for a result of bench.py, by default its platform's
(benchplatforms.py): docs/benchmarks/history/<platform>.jsonl, e.g. linux-x86-64.jsonl; `live`
appends one line (default docs/benchmarks/history/macos-arm64-live.jsonl) for the files
live_latency.py wrote with LATENCY_JSON, one per program. A line keeps the medians and spreads
of each phase, not every sample: the full result is the workflow's artifact, and the latest of
each platform is docs/benchmarks/<platform>.json. `suite:"merge"` writes the line of the
per-merge run (bench-counts.yaml: the stress corpus and a small project, default
docs/benchmarks/history/<platform>-merge.jsonl), which the site does not chart.

A historical build line (schema 2 omits every Cargo field and adds compiler identity):
    {"suite": "build", "date", "commit", "platform", "machine", "cpu", "cores", "runs", "run_url",
     "results": {"<project>/<backend>": {"tag", "store_mb", "runtime_s": {"blaze", "cargo debug", "cargo release"},
                 "runtime_mb": {"blaze", "cargo debug", "cargo release"},
                 "resident_ready_mb", "resident_end_mb", "builds", "disk_mb", "cargo_disk_mb",
                 "phases": {"<phase>": {"wall_s", "cargo_s", "min_s", "max_s", "cargo_min_s", "cargo_max_s",
                                        "rss_mb", "mem_mb", "cargo_mem_mb", "resident_mb", "built", "link",
                                        "compiled", "build_s", "warm_s", "setups": {"<setup>": seconds},
                                        "instructions", "cycles", "counts_gap", "warm_store_instructions"}}}}}
`instructions` and `cycles` are the medians of what blaze's processes retired over the phase
(bench.py, harness/counters.py), null with `counts_gap` saying why where they were not counted.
A result's stress corpus (bench.py `stress:true`) is kept as `stress:<crate>/<backend>` entries
with the phases `check` and `build`.
Memory (bench.py's `memory`, docs/benchmarks.md "Memory and disk"): `mem_mb` and `cargo_mem_mb` are
the peak of each side during the phase, `resident_mb` what the daemon held after it;
`resident_ready_mb` and `resident_end_mb` what it held once ready and after the `builds` builds
of the run, `disk_mb` blaze's target directory, store and home, `cargo_disk_mb` cargo's target.
A live line:
    {"suite": "live", "date", "commit", "machine", "battery", "run_url",
     "results": {"<program>": {"<mode>": {"n", "p50_ms", "p90_ms", "p95_ms", "p99_ms", "max_ms"}}}}
Keys a run did not measure are left out. A run's test suites (`tests:true`) are two phases:
`test-edit`, the tests after an edit, and `test`, with nothing changed.

`check` compares the last line of HISTORY with the lines before it from the same machine (at
most `last`, default 7): a number regressed when it is above the median of those by more than
its noise band and by more than its floor (1M instructions, 50 ms for build times, 1 ms for live
latency, 100 MB of memory, 50 MB of store and disk), and improved when below it by as much.
Instructions are the primary signal, as in rustc-perf: their band is `instructions_noise`
percent (default 1). Every other number is secondary and keeps the band of `noise` percent
(default 10). Either band is that or three times the spread the history shows (1.4826 times the
median absolute deviation, as a percent of the median), whichever is larger; with fewer than
three earlier lines it is twice the percent. Memory checked is blaze's: its peak in each phase,
the daemon after each phase, once ready and at the end of the run, and its disk; cargo's is
charted, not checked. A number with fewer than `least` earlier values (default 1) is not
checked: on a machine whose own runs differ by more than that band, as a hosted runner's do, one
or two runs say nothing of its noise. A phase the daemon built that cargo now builds is a
regression whatever the times. The regressions, primary first, are written to stdout and, as
markdown, to `report`, after a summary in rustc-perf's form: for instructions, wall time and
peak memory, how many numbers regressed and improved past their band and the geometric mean of
their change, and the same over every number compared, below the band too. The exit status is 1
when anything regressed, and 0 with nothing to compare against.
"""
import datetime, json, math, os, statistics, sys
from dataclasses import dataclass
from typing import Literal

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import benchschema
from benchplatforms import platform, slug

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
HISTORY = os.path.join(ROOT, "docs/benchmarks/history")
LIVE_HISTORY = os.path.join(HISTORY, "macos-arm64-live.jsonl")
PHASE_FIELDS = ("wall_s", "cargo_s", "rss_mb", "built", "link", "compiled", "build_s", "warm_s",
                "warm_store_s", "warm_store_build_s", "check_s", "instructions", "cycles", "counts_gap",
                "warm_store_instructions")
# The suites whose lines are builds: the full run's, and the per-merge run's.
BUILD_SUITES = ("build", "merge")
# What is checked, and the least change that counts.
FLOORS = {"wall_s": 0.05, "build_s": 0.05, "warm_s": 0.05, "rss_mb": 100, "store_mb": 50,
          "mem_mb": 100, "resident_mb": 100, "resident_ready_mb": 100, "resident_end_mb": 100, "disk_mb": 50,
          "p50_ms": 1.0, "p90_ms": 1.0, "blaze": 0.005, "instructions": 1_000_000}
# The primary signal: instructions, judged with their own, tighter band.
PRIMARY = ("instructions",)
# The kinds of number the summary counts, by field.
KINDS = {"instructions": "instructions", "wall_s": "wall time", "build_s": "wall time", "warm_s": "wall time",
         "p50_ms": "wall time", "p90_ms": "wall time", "mem_mb": "peak memory", "rss_mb": "peak memory"}


@dataclass(frozen=True)
class Build:
    result: str
    action: Literal["build"] = "build"
    history: str | None = None
    suite: Literal["build", "merge"] = "build"
    run_url: str | None = None

    def __post_init__(self):
        if not self.result:
            raise ValueError("result must name a file")


@dataclass(frozen=True)
class Live:
    files: tuple[str, ...]
    action: Literal["live"] = "live"
    history: str | None = None
    machine: str | None = None
    run_url: str | None = None

    def __post_init__(self):
        if not self.files or any(not path for path in self.files):
            raise ValueError("files must name at least one latency result")


@dataclass(frozen=True)
class Check:
    history: str
    action: Literal["check"] = "check"
    last: int = 7
    least: int = 1
    noise: int | float = 10
    instructions_noise: int | float = 1
    report: str | None = None

    def __post_init__(self):
        if not self.history:
            raise ValueError("history must name a file")
        if self.last < 1 or self.least < 1:
            raise ValueError("last and least must be positive")
        if not benchschema.number(self.noise) or not benchschema.number(self.instructions_noise):
            raise ValueError("noise and instructions_noise must be finite and nonnegative")


def load(text):
    return benchschema.load_input(text, {"build": Build, "live": Live, "check": Check}, "build", "benchhistory.py")


def spread(samples, prefix=""):
    if not samples:
        return {}
    return {f"{prefix}min_s": round(min(samples), 3), f"{prefix}max_s": round(max(samples), 3)}


def build_line(result, suite="build", run_url=None):
    if wrong := benchschema.errors(result):
        raise ValueError("cannot append incomplete benchmark evidence: " + "; ".join(wrong))
    host = result.get("host") or {}
    line = dict(suite=suite, date=result["date"], commit=result.get("commit"), platform=platform(result), machine=host.get("name"),
                cpu=host.get("cpu"), cores=host.get("cores") or result.get("cores"), runs=result.get("runs"),
                run_url=result.get("run_url") or run_url, results={})
    if benchschema.native(result):
        line.update(schema=result["schema"], compiler=result["compiler"], compiler_after=result["compiler_after"],
                    source_tree=result.get("source_tree"))
    measured_all = dict(result["results"])
    measured_all.update((f"stress:{key}", measured) for key, measured in (result.get("stress") or {}).items())
    if result.get("stress_gap"):
        line["stress_gap"] = result["stress_gap"]
    for key, measured in measured_all.items():
        phases = {}
        for label, phase in measured["phases"].items():
            kept = {f: phase[f] for f in PHASE_FIELDS if phase.get(f) is not None}
            kept.update(spread(phase.get("samples_s")))
            kept.update(spread(phase.get("cargo_samples_s"), "cargo_"))
            setups = {name: s["median_s"] for name, s in (phase.get("setups") or {}).items() if s.get("median_s") is not None}
            if setups:
                kept["setups"] = setups
            held = phase.get("memory") or {}
            for field, side, number in (("mem_mb", "blaze", "peak_mb"), ("cargo_mem_mb", "cargo", "peak_mb"), ("resident_mb", "blaze", "resident_mb")):
                if (held.get(side) or {}).get(number) is not None:
                    kept[field] = held[side][number]
            phases[label] = kept
            if label == "test" and phase.get("edit_s") is not None:
                # The tests after an edit are a phase of their own, as charted and checked.
                edited = dict(wall_s=phase["edit_s"], built=phase.get("built"))
                if phase.get("edit_cargo_s") is not None:
                    edited["cargo_s"] = phase["edit_cargo_s"]
                edited.update(spread(phase.get("edit_samples_s")))
                edited.update(spread(phase.get("edit_cargo_samples_s"), "cargo_"))
                phases["test-edit"] = edited
                kept["built"] = phase.get("built_still", phase.get("built"))
        entry = dict(tag=measured.get("tag"), phases=phases)
        if measured.get("store_mb") is not None:
            entry["store_mb"] = measured["store_mb"]
        if measured.get("runtime"):
            entry["runtime_s"] = {label: r["median_s"] for label, r in measured["runtime"].items() if isinstance(r, dict) and "median_s" in r}
            peaks = {label: r["peak_mb"] for label, r in measured["runtime"].items() if isinstance(r, dict) and r.get("peak_mb") is not None}
            if peaks:
                entry["runtime_mb"] = peaks
        resident, disk = measured.get("resident") or {}, measured.get("disk_mb") or {}
        for field, value in (("resident_ready_mb", resident.get("ready_mb")), ("resident_end_mb", resident.get("end_mb")),
                             ("builds", resident.get("builds")),
                             ("disk_mb", sum(disk.get(k) or 0 for k in ("target", "store", "home")) if disk else None),
                             ("cargo_disk_mb", disk.get("cargo target"))):
            if value is not None:
                entry[field] = value
        line["results"][key] = entry
    return line


def percentile(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))]


def live_line(files, machine, run_url=None):
    line = dict(suite="live", results={})
    for path in files:
        latency = json.load(open(path))
        if "modes" not in latency:
            raise SystemExit(f"{path}: not a live_latency.py result with metadata (no `modes`)")
        for field in ("date", "commit", "run_url"):
            if latency.get(field) and not line.get(field):
                line[field] = latency[field]
        host = latency.get("host") or {}
        line.setdefault("machine", machine or host.get("name"))
        line.setdefault("battery", host.get("battery"))
        program = line["results"].setdefault(latency["program"], {})
        for mode, rows in latency["modes"].items():
            totals = [row["total"] for row in rows if isinstance(row.get("total"), (int, float))]
            if totals:
                program[mode] = dict(n=len(totals), **{f"p{p}_ms": round(percentile(totals, p), 2) for p in (50, 90, 95, 99)},
                                     max_ms=round(max(totals), 2))
    line.setdefault("date", time_now())
    line.setdefault("run_url", run_url)
    return line


def time_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def append(path, line):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        f.write(json.dumps(line, separators=(",", ":")) + "\n")
    print(f"appended {line['suite']} {line.get('commit') or ''} {line['date']} to {path}")


def series(line):
    """Every number of a line that is checked, by name, and the `built` of each phase."""
    numbers, built = {}, {}
    for key, entry in line["results"].items():
        if line["suite"] in BUILD_SUITES:
            if entry.get("store_mb") is not None:
                numbers[(key, "store", "store_mb")] = entry["store_mb"]
            if (entry.get("runtime_s") or {}).get("blaze") is not None:
                numbers[(key, "runtime", "blaze")] = entry["runtime_s"]["blaze"]
            for field in ("resident_ready_mb", "resident_end_mb", "disk_mb"):
                if entry.get(field) is not None:
                    numbers[(key, "daemon" if field.startswith("resident") else "disk", field)] = entry[field]
            for label, phase in entry["phases"].items():
                for field in ("instructions", "wall_s", "rss_mb", "mem_mb", "resident_mb", "build_s", "warm_s"):
                    if phase.get(field) is not None:
                        numbers[(key, label, field)] = phase[field]
                if phase.get("built"):
                    built[(key, label)] = phase["built"]
        else:
            for mode, stats in entry.items():
                for field in ("p50_ms", "p90_ms"):
                    if stats.get(field) is not None:
                        numbers[(key, mode, field)] = stats[field]
    for name, value in numbers.items():
        if not benchschema.number(value):
            raise ValueError(f"{' '.join(name)}: history value is not finite and nonnegative")
    return numbers, built


def band_of(history, percent):
    """The noise band of a number, in percent: `percent`, or three times the spread `history`
    shows, whichever is larger; twice `percent` with fewer than three values."""
    median = statistics.median(history)
    if len(history) >= 3 and median > 0:
        mad = statistics.median(abs(h - median) for h in history) * 1.4826
        return max(percent, 300 * mad / median)
    return 2 * percent


def compare(lines, last, noise, least=1, instructions_noise=1.0):
    """The last of `lines` against the median of the ones before it from the same machine: every
    number compared, each with its verdict (`regressed`, `improved` or `within` its band), and
    how many lines it was compared against."""
    if not lines:
        raise ValueError("history contains no measurements")
    now = lines[-1]
    numbers, built = series(now)
    same = [l for l in lines[:-1] if l.get("suite") == now.get("suite")
            and l.get("machine") == now.get("machine") and l.get("cores") == now.get("cores")][-last:]
    if not same:
        return [], 0
    earlier = [series(l) for l in same]
    compared = []
    for name in sorted({name for values, _ in earlier for name in values} - numbers.keys()):
        history = [values[name] for values, _ in earlier if name in values]
        if len(history) >= max(least, 1):
            compared.append(dict(series=" ".join(name), kind=KINDS.get(name[2]), primary=name[2] in PRIMARY,
                                 was=statistics.median(history), now="not measured", percent=None, band=None, verdict="regressed"))
    for name, value in sorted(numbers.items()):
        history = [n[name] for n, _ in earlier if name in n]
        if len(history) < max(least, 1):
            continue
        field = name[2]
        primary = field in PRIMARY
        median = statistics.median(history)
        band = band_of(history, instructions_noise if primary else noise)
        floor = FLOORS.get(field, 0)
        if value > median * (1 + band / 100) and value - median > floor:
            verdict = "regressed"
        elif value < median * (1 - band / 100) and median - value > floor:
            verdict = "improved"
        else:
            verdict = "within"
        compared.append(dict(series=" ".join(name), kind=KINDS.get(field), primary=primary, was=median, now=value,
                             percent=round(100 * (value - median) / median, 2 if primary else 0) if median else None,
                             band=round(band, 2 if primary else 0), verdict=verdict))
    for name in sorted(set(built) | {name for _, values in earlier for name in values}):
        how = built.get(name, "not measured")
        was = [b[name] for _, b in earlier if name in b]
        if was and was[-1] == "daemon" and how != "daemon":
            compared.append(dict(series=" ".join(name) + " built", kind=None, primary=False, was="daemon", now=how,
                                 percent=None, band=None, verdict="regressed"))
    return compared, len(same)


def regressions(lines, last, noise, least=1, instructions_noise=1.0):
    """What regressed in the last of `lines`, primary first, and how many lines it was compared against."""
    compared, n = compare(lines, last, noise, least, instructions_noise)
    found = [c for c in compared if c["verdict"] == "regressed"]
    return sorted(found, key=lambda c: not c["primary"]), n


def geomean_change(rows):
    """The geometric mean of now / was over `rows`, as a percent change; None for no rows."""
    ratios = [r["now"] / r["was"] for r in rows if isinstance(r["now"], (int, float)) and isinstance(r["was"], (int, float))
              and r["was"] > 0 and r["now"] > 0]
    if not ratios:
        return None
    return round(100 * (math.exp(sum(map(math.log, ratios)) / len(ratios)) - 1), 2)


def summary(compared):
    """rustc-perf's summary, for each kind: past the band, how many regressed and improved and
    the geometric mean of their change; over every number compared (below the band too), how
    many went up and down and the geometric mean of all their changes."""
    out = {}
    for kind in dict.fromkeys(KINDS.values()):
        rows = [c for c in compared if c["kind"] == kind]
        if not rows:
            continue
        regressed = [r for r in rows if r["verdict"] == "regressed"]
        improved = [r for r in rows if r["verdict"] == "improved"]
        numeric = [r for r in rows if isinstance(r["now"], (int, float)) and isinstance(r["was"], (int, float))]
        out[kind] = dict(regressed=len(regressed), regressed_mean=geomean_change(regressed), improved=len(improved),
                         improved_mean=geomean_change(improved), compared=len(rows),
                         up=sum(r["now"] > r["was"] for r in numeric), down=sum(r["now"] < r["was"] for r in numeric),
                         all_mean=geomean_change(rows))
    return out


def summary_table(totals):
    def pct(v):
        return "" if v is None else f"{v:+.2f}%"

    lines = ["| Kind | Regressed past band | Mean | Improved past band | Mean | All: up / down of compared | All: mean |",
             "|---|---:|---:|---:|---:|---:|---:|"]
    for kind, t in totals.items():
        lines.append(f"| {kind} | {t['regressed']} | {pct(t['regressed_mean'])} | {t['improved']} | {pct(t['improved_mean'])} | "
                     f"{t['up']} / {t['down']} of {t['compared']} | {pct(t['all_mean'])} |")
    return "\n".join(lines)


def report(now, found, compared, least=1, totals=None):
    head = f"{now['suite']} benchmarks on {now.get('machine')}, blaze {(now.get('commit') or '?')[:10]}, {now['date']}"
    if now.get("run_url"):
        head += f" ([run]({now['run_url']}))"
    if compared < least:
        return f"{head}: not checked, {compared} runs before it on this machine of the {least} its noise is told from."
    table = f"\n\n{summary_table(totals)}" if totals else ""
    if not found:
        return f"{head}: nothing regressed against the {compared} runs before it.{table}"
    lines = [f"{head}: {len(found)} regressed against the median of the {compared} runs before it.{table}", "",
             "| What | Signal | Before | Now | Change | Noise band |", "|---|---|---:|---:|---:|---:|"]
    for f in found:
        change = f"+{f['percent']}%" if f["percent"] is not None else ""
        band = f"{f['band']}%" if f["band"] is not None else ""
        signal = "primary" if f["primary"] else "secondary"
        lines.append(f"| {f['series']} | {signal} | {f['was']} | {f['now']} | {change} | {band} |")
    return "\n".join(lines)


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    given = load(sys.argv[1])
    if isinstance(given, Build):
        result = json.load(open(given.result))
        line = build_line(result, given.suite, given.run_url)
        default = f"{slug(platform(result))}{'' if given.suite == 'build' else '-' + given.suite}.jsonl"
        append(given.history or os.path.join(HISTORY, default), line)
    elif isinstance(given, Live):
        append(given.history or LIVE_HISTORY, live_line(given.files, given.machine, given.run_url))
    else:
        lines = [json.loads(line) for line in open(given.history) if line.strip()]
        rows, compared = compare(lines, given.last, given.noise, given.least, given.instructions_noise)
        found = sorted((c for c in rows if c["verdict"] == "regressed"), key=lambda c: not c["primary"])
        text = report(lines[-1], found, compared, given.least, summary(rows))
        print(text)
        if given.report:
            with open(given.report, "w") as output:
                output.write(text + "\n")
        return 1 if found else 0
    return 0


if __name__ == "__main__":
    sys.path.insert(0, os.path.join(ROOT, "tools", "typed-input", "src"))
    raise SystemExit(main())
