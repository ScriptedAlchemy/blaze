#!/usr/bin/env python3
"""The binary cache in nix-live.yaml is optional; every Nix gate is not.

  python3 harness/dist/check_nix_workflow.py [workflow]

FlakeHub Cache wants a flake registered with FlakeHub and an OIDC token this repository does
not grant, so the cache step is off unless the repository variable ENABLE_FLAKEHUB_CACHE is
"true", and it alone carries `continue-on-error`. That is only safe while the work it would
speed up is still there and still unconditional, so this fails when:

  - an expected job or an expected command is missing (a deleted gate is not a passing gate);
  - an expected command sits on a step with `if:` or `continue-on-error`, or in a job that is
    conditional or allowed to fail (except `live-edits`, which the file already marked not
    required before this change);
  - `continue-on-error` appears on a step that is not a cache step;
  - a cache step stops being gated on ENABLE_FLAKEHUB_CACHE, or may fail a job.

This is a structural check of the workflow file, not a Nix run: it proves the required
commands are present and unconditional, and it cannot prove they succeed. It runs as its own
step in the `unit-graphs` job (`.github/workflows/nix-live.yaml`), so a change that drops a
gate is reported by the run that still has this step. A change that deletes this step too
cannot police itself; review sees that deletion in the diff.

Stdlib only.
"""

import re
import sys
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parent.parent.parent / ".github/workflows/nix-live.yaml"
# (job, command substring) pairs the workflow must run. Each names a gate: the graph
# regeneration and its freshness diff, flake evaluation, the Linux and Darwin builds, the
# binaries' self-run check, the demo, and the live-edit run. Editing this list is how a gate
# would be dropped, so it is reviewed like the workflow.
EXPECTED = [
    ("unit-graphs", "python3 nix/unit_graph.py"),
    ("unit-graphs", "git diff --exit-code -- nix/units"),
    ("nix", "nix flake check"),
    ("nix", "nix build --print-build-logs --no-link .#daemon"),
    ("nix", "nix run .#blaze"),
    ("nix", "for attr in daemon daemon-units"),
    ("nix-darwin", "nix eval --raw"),
    ("nix-darwin", "nix build --print-build-logs --no-link .#daemon-units"),
    ("demo", "cargo test --release -p ra-jit-live"),
    ("demo", "xvfb-run"),
    ("live-edits", "python3 harness/live_e2e.py"),
]
CACHE_ACTIONS = ("DeterminateSystems/flakehub-cache-action", "DeterminateSystems/magic-nix-cache-action")
# Jobs the file already allowed to fail, with the reason it gives. Exempt from the
# unconditional rule only; the commands above must still be present in them.
EXEMPT = {
    # "Not required: it runs a daemon, a program and a patch between them on a hosted macOS
    # VM, which has been the least steady thing in the repository."
    "live-edits": "hosted macOS live-edit run, marked not required before this change",
}


class Bad(Exception):
    pass


def indent_of(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def scan(text: str) -> dict[str, dict]:
    """{job: {"if": bool, "continue": bool, "steps": [step]}}.

    A step is {"lines": [...], "indent": n}. Comments and blank lines are dropped from a step's
    lines so `#` inside a step cannot hide a key. A shape the scanner does not know raises Bad,
    so a new construct cannot silently pass.
    """
    jobs: dict[str, dict] = {}
    lines = text.splitlines()
    i = 0
    while i < len(lines) and not re.match(r"^jobs:\s*$", lines[i]):
        i += 1
    if i == len(lines):
        raise Bad("no `jobs:` block")
    i += 1
    job = None
    while i < len(lines):
        line = lines[i]
        if not line.strip() or line.lstrip().startswith("#"):
            i += 1
            continue
        ind = indent_of(line)
        if ind == 0:
            break
        if ind == 2 and re.match(r"^  [\w-]+:\s*$", line):
            job = line.strip().rstrip(":")
            jobs[job] = {"if": False, "continue": False, "steps": []}
            i += 1
            continue
        if job is None:
            raise Bad(f"line {i + 1}: content before a job name: {line!r}")
        if re.match(r"^    continue-on-error:", line):
            jobs[job]["continue"] = True
        elif re.match(r"^    if:", line):
            jobs[job]["if"] = True
        elif re.match(r"^    steps:\s*$", line):
            i = read_steps(lines, i + 1, jobs[job]["steps"])
            continue
        elif ind <= 4 and not re.match(r"^    [\w./-]+:", line):
            raise Bad(f"line {i + 1}: unknown job key: {line!r}")
        i += 1
    if not jobs:
        raise Bad("no jobs")
    return jobs


def read_steps(lines: list[str], i: int, out: list[dict]) -> int:
    """Read `steps:` entries into out; return the line after them."""
    while i < len(lines):
        line = lines[i]
        if not line.strip() or line.lstrip().startswith("#"):
            i += 1
            continue
        ind = indent_of(line)
        if ind <= 4:
            return i
        if not re.match(r"^      - ", line):
            raise Bad(f"line {i + 1}: expected a step: {line!r}")
        start = i
        i += 1
        while i < len(lines):
            nxt = lines[i]
            if not nxt.strip():
                i += 1
                continue
            if indent_of(nxt) <= 6:
                break
            i += 1
        kept = [ln for ln in lines[start:i] if ln.strip() and not ln.lstrip().startswith("#")]
        out.append({"lines": kept})
    return i


def step_body(step: dict) -> str:
    """The step's `run:` text, folded to one string per line; "" when it has no run."""
    text = "\n".join(step["lines"])
    m = re.search(r"^\s+(?:- )?run:\s*(.*)$", text, re.MULTILINE)
    if not m:
        return ""
    if m.group(1).strip() not in ("|", ">", ""):
        return m.group(1)
    rest = text[m.end():]
    return "\n".join(ln.strip() for ln in rest.splitlines() if ln.strip())


def step_name(step: dict) -> str:
    for ln in step["lines"]:
        m = re.match(r"^\s+(?:- )?name:\s*(.+)$", ln)
        if m:
            return m.group(1).strip().strip("\"'")
    for ln in step["lines"]:
        m = re.match(r"^\s+(?:- )?uses:\s*(\S+)", ln)
        if m:
            return m.group(1)
    body = step_body(step)
    return body.splitlines()[0][:40] if body else "a step"


def step_key(step: dict, key: str) -> bool:
    return any(re.match(rf"^\s+(?:- )?{key}:", ln) for ln in step["lines"])


def step_is_cache(step: dict) -> bool:
    text = "\n".join(step["lines"])
    return any(a in text for a in CACHE_ACTIONS)


def check(text: str) -> list[str]:
    """Every problem, as a line of text; empty when the workflow is as required."""
    problems = []
    jobs = scan(text)
    for job, cmd in EXPECTED:
        j = jobs.get(job)
        if j is None:
            problems.append(f"{job}: job is missing but runs required work")
            continue
        hits = [s for s in j["steps"] if cmd in step_body(s)]
        if not hits:
            problems.append(f"{job}: required command {cmd!r} is missing")
            continue
        for s in hits:
            name = step_name(s)
            if step_key(s, "if"):
                problems.append(f"{job}: required step {name!r} ({cmd}) is conditional")
            if step_key(s, "continue-on-error"):
                problems.append(f"{job}: required step {name!r} ({cmd}) may fail")
        if j["if"]:
            problems.append(f"{job}: the job is conditional but runs required work")
        if j["continue"] and job not in EXEMPT:
            problems.append(f"{job}: the job may fail but runs required work")
    for job, j in jobs.items():
        for s in j["steps"]:
            if step_key(s, "continue-on-error") and not step_is_cache(s):
                problems.append(f"{job}: {step_name(s)!r} carries continue-on-error but is not a cache step")
    for job, j in jobs.items():
        for s in j["steps"]:
            if not step_is_cache(s):
                continue
            if not step_key(s, "continue-on-error"):
                problems.append(f"{job}: the cache step may fail a job")
            if "ENABLE_FLAKEHUB_CACHE" not in "\n".join(s["lines"]):
                problems.append(f"{job}: the cache step is not gated on ENABLE_FLAKEHUB_CACHE")
    return problems


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else WORKFLOW
    try:
        problems = check(path.read_text())
    except Bad as e:
        print(f"{path}: cannot check: {e}", file=sys.stderr)
        return 1
    for p in problems:
        print(f"{path}: {p}", file=sys.stderr)
    if problems:
        return 1
    print(f"{path}: the cache is optional; every required command is present and unconditional")
    return 0


if __name__ == "__main__":
    sys.exit(main())
