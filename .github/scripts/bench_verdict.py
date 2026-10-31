#!/usr/bin/env python3
"""Judge the paired tango runs of the `tango` job; run no benchmark and compile nothing.

The job runs tango three ways on one runner: bench-base against a copy of itself (the A/A
control), then bench-head against bench-base, once and once more if that one reported a
slowdown. Tango's own exit status says only that some benchmark was slower in a run. The verdict
here is that of `bench.yaml`'s header comment:

  environmental  the A/A control, the same binary against itself, shows a benchmark over the
                 threshold: the runner cannot tell a slowdown from itself, so the comparison with
                 the head is reported and not counted (exit 0);
  regression      the same benchmark is over the threshold in both comparisons (exit 1);
  noise           a benchmark was over the threshold in the first and a different one in the
                 second, or none in the second (exit 0);
  pass            nothing was over the threshold (exit 0);
  error           the last comparison measured nothing (exit 1).

"Over the threshold" is tango's own: the mean slower by more than `--noise-threshold` percent
and significant, which tango marks `*` (its exit status is the same test). The text is read in
tango's `-v` form (`--color never`).
"""
import argparse
import re
import sys
from pathlib import Path

BLOCK = re.compile(r"^(\S+)\s+\(n: \d+, outliers: \d+\)\s*$")
MEAN = re.compile(
    r"^\s+mean\s+│\s+(\S+ \S+)\s+(\S+ \S+)\s+(\S+ \S+)\s+([+-]\d+(?:\.\d+)?)%(\*?)\s*$"
)


def means(text):
    """Each benchmark's mean line: {name: (baseline, candidate, percent, significant)}."""
    found, name = {}, None
    for line in text.splitlines():
        block = BLOCK.match(line)
        if block:
            # Duplicate results or a header without its mean indicate incomplete
            # evidence; never let a later block overwrite a previous measurement.
            if name is not None or block.group(1) in found:
                return {}
            name = block.group(1)
            continue
        mean = MEAN.match(line)
        if mean and name is not None:
            base, candidate, _, pct, star = mean.groups()
            found[name] = (base, candidate, float(pct), star == "*")
            name = None
    return found if name is None else {}


def slow(text):
    """The benchmarks tango reports significantly slower in the candidate."""
    return {name: m for name, m in means(text).items() if m[3] and m[2] > 0}


def show(slowed):
    if not slowed:
        return "none"
    return "; ".join(
        f"{name} {pct:+.2f}% ({base} -> {candidate})"
        for name, (base, candidate, pct, _) in sorted(slowed.items())
    )


def cpu_line(text):
    """The runner's CPU and the pair's digests, as the job's `cpu.txt` has them."""
    model = count = None
    digests = []
    for line in text.splitlines():
        if line.startswith("Model name:"):
            model = line.split(":", 1)[1].strip()
        elif line.startswith("CPU(s):"):
            count = line.split(":", 1)[1].strip()
        elif re.match(r"^[0-9a-f]{64}\s", line):
            digest, path = line.split(None, 1)
            digests.append(f"{Path(path.strip()).name} {digest[:12]}")
    cpu = f"{model or 'unknown CPU'} ({count or '?'} CPUs)"
    return cpu + ("; " + ", ".join(digests) if digests else "")


def judge(aa, attempts, cpu="", aa_seconds=None, exit_codes=None, aa_exit_code=None):
    """The verdict word, the report and the exit status for the texts of one job."""
    control = means(aa)
    measured = [means(text) for text in attempts]
    expected = set(control) or next((set(rows) for rows in measured if rows), set())
    aa_slow = slow(aa)
    parsed = [bool(rows) and set(rows) == expected for rows in measured]
    slows = [slow(text) for text in attempts]
    lines = []
    if cpu:
        lines.append(f"runner: {cpu}")
    took = f", {aa_seconds} s" if aa_seconds is not None else ""
    if means(aa):
        lines.append(f"A/A control (bench-base against a copy of itself{took}): over the threshold: {show(aa_slow)}")
    else:
        lines.append(f"A/A control (bench-base against a copy of itself{took}): measured nothing")
    for number, (ok, slowed) in enumerate(zip(parsed, slows), 1):
        lines.append(f"comparison {number}: " + (f"over the threshold: {show(slowed)}" if ok else "measured nothing"))

    process_failed = exit_codes is not None and (
        len(exit_codes) != len(attempts) or not exit_codes
        or exit_codes[-1] not in (0, 1) or (exit_codes[-1] == 1 and not slows[-1]))
    if process_failed:
        lines.append(f"comparison process exit statuses: {exit_codes}")
    if process_failed or not parsed or not parsed[-1]:
        verdict, status = "error", 1
        lines.append("last comparison has missing, duplicate, or incomplete measurements")
    else:
        # A comparison that measured nothing says nothing about which benchmark was slow.
        repeated = set(slows[-1])
        if len(attempts) > 1 and parsed[0]:
            repeated &= set(slows[0])
        if repeated:
            verdict, status = "regression", 1
        elif any(slows):
            verdict, status = "noise", 0
        else:
            verdict, status = "pass", 0
        if len(attempts) > 1 and repeated:
            lines.append("over the threshold in both comparisons: " + ", ".join(sorted(repeated)))
    if aa_slow and aa_exit_code in (None, 0, 1) and verdict != "error":
        note = f", where it would have been {verdict}" if verdict in ("regression", "noise") else ""
        verdict, status = "environmental", 0
        lines.append(
            "the same binary is over the threshold against itself on this runner: "
            f"the comparison with the base is not counted{note}"
        )
    lines.insert(0, f"tango verdict: {verdict}")
    return verdict, "\n".join(lines), status


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--aa", required=True, help="tango's output of bench-base against its copy")
    parser.add_argument("--aa-seconds", type=int, help="how long the A/A control took")
    parser.add_argument("--cpu-file", help="the job's cpu.txt")
    parser.add_argument("--exit-code", type=int, action="append", help="one exit status per comparison")
    parser.add_argument("--aa-exit-code", type=int, help="the A/A process exit status")
    parser.add_argument("attempts", nargs="+", help="tango's output of each comparison, in order")
    args = parser.parse_args(argv)
    cpu = ""
    if args.cpu_file and Path(args.cpu_file).is_file():
        cpu = cpu_line(Path(args.cpu_file).read_text())
    _, report, status = judge(
        Path(args.aa).read_text(), [Path(it).read_text() for it in args.attempts], cpu, args.aa_seconds,
        args.exit_code, args.aa_exit_code
    )
    print(report)
    return status


if __name__ == "__main__":
    sys.exit(main())
