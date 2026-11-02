#!/usr/bin/env python3
"""The binary cache in nix-live.yaml is optional; every Nix gate is not.

  python3 harness/dist/check_nix_workflow.py [workflow]

FlakeHub Cache wants a flake registered with FlakeHub and an OIDC token this repository does
not grant, so the cache step is off unless the repository variable ENABLE_FLAKEHUB_CACHE is
true, and it alone carries `continue-on-error: true`. That is only safe while the work it
would speed up is still there and still unconditional, so this fails when:

  - an expected job or an expected command is missing — a deleted gate is not a passing gate;
  - an expected command is only *mentioned* rather than run (`echo "nix flake check"` is an
    argument, not a gate);
  - an expected command sits on a step with `if:` or a truthy `continue-on-error:`, or in a
    job that is conditional or allowed to fail (except `live-edits`, which the file already
    marked not required before this change);
  - a step that is not the cache carries a truthy `continue-on-error:`;
  - a Nix consumer allocates the fork's shared 10 GB Actions cache (reserved for
    the central bundle producer): a cache action, or the toolchain action's `cache:`
    without `cache-save: false` (a read-only restore of the producer's entry is fine);
  - a cache step's `if:` is not exactly the supported opt-in predicate, or its
    `continue-on-error:` is not literally `true`, or a cache step is not gated at all.

Two bounded readers do that, and neither is a general YAML or shell implementation:

  - `scan` reads the jobs/steps subset these workflows use, keeping a step's real keys and
    refusing a shape it does not know rather than guessing.
  - `segments`/`tokens` split a `run:` body into simple commands at real command positions
    (quotes, `$(...)`, `;`/`&&`/`||`/`|`, comments) and drop redirections and the `sudo`/
    `timeout N`/`env`/`!` prefixes this file uses. Its purpose is only to tell a command that
    runs from the spelling of one inside an argument. A shell construct it does not model
    yields no command match, so an unknown form cannot count as a gate.

This is a structural check of the workflow file, not a Nix run: it proves the required
commands are present at command positions and unconditional, and it cannot prove they
succeed. It runs as its own step in the `unit-graphs` job, so a change that drops a gate is
reported by the run that still has this step. A change that deletes this step too cannot
police itself; review sees that deletion in the diff.

Stdlib only.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parent.parent.parent / ".github/workflows/nix-live.yaml"
# (job, command) pairs the workflow must run. Each names a gate: the graph regeneration and
# its freshness diff, flake evaluation, the Linux and Darwin builds, the binaries' self-run
# check, the demo, and the live-edit run. Editing this list is how a gate would be dropped,
# so it is reviewed like the workflow. A command matches only at a command position.
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
# The central producer alone owns the fork's bounded Actions-cache allocation.
# FlakeHub is an external service and remains subject to its explicit opt-in below.
ACTIONS_CACHE_ACTIONS = (
    "actions/cache", "nix-community/cache-nix-action", "Swatinem/rust-cache",
    "mozilla-actions/sccache-action", "DeterminateSystems/magic-nix-cache-action",
)
CACHE_ACTIONS = ("DeterminateSystems/flakehub-cache-action", "DeterminateSystems/magic-nix-cache-action")
# The one opt-in predicate the cache may carry. Anything else -- `always() || ...`, a bare
# identifier, a different variable -- runs the cache without the opt-in and is refused.
OPT_IN = {
    "vars.ENABLE_FLAKEHUB_CACHE == 'true'",
    'vars.ENABLE_FLAKEHUB_CACHE == "true"',
}
# Jobs the file already allowed to fail, with the reason it gives. Exempt from the
# unconditional rule only; the commands above must still be present.
EXEMPT = {
    # "Not required: it runs a daemon, a program and a patch between them on a hosted macOS
    # VM, which has been the least steady thing in the repository."
    "live-edits": "hosted macOS live-edit run, marked not required before this change",
}


class Bad(Exception):
    pass


def indent_of(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


# --- the workflow reader -------------------------------------------------------------


def scan(text: str) -> dict[str, dict]:
    """{job: {"if": bool, "continue": bool, "steps": [step]}}.

    A step is {"lines": [...]}. Comment and blank lines are dropped from a step's lines so a
    `#` inside a step cannot hide a key. A shape the scanner does not know raises Bad, so a
    new construct cannot silently pass.
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
            jobs[job]["continue"] = truthy(scalar(line.split(":", 1)[1]))
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
    """The step's `run:` text, one line per line; "" when it has no run."""
    text = "\n".join(step["lines"])
    m = re.search(r"^\s+(?:- )?run:\s*(.*)$", text, re.MULTILINE)
    if not m:
        return ""
    if m.group(1).strip() not in ("|", ">", ""):
        return m.group(1)
    rest = text[m.end():]
    return "\n".join(ln.strip() for ln in rest.splitlines() if ln.strip())


def scalar(raw: str) -> str:
    """A YAML scalar's value, without quotes or a trailing comment."""
    v = raw.strip()
    if v and v[0] not in "'\"":
        v = re.sub(r"\s+#.*$", "", v)
    return v.strip()


def step_value(step: dict, key: str) -> str | None:
    """The step's value for `key` (last one wins), or None when the key is absent."""
    found = None
    for ln in step["lines"]:
        m = re.match(rf"^\s+(?:- )?{key}:\s*(.*)$", ln)
        if m:
            found = scalar(m.group(1))
    return found


def truthy(value: str | None) -> bool:
    """Whether a YAML boolean-ish value allows something. Unknown expressions count."""
    if value is None:
        return False
    v = value.strip().strip("\"'").lower()
    return v not in ("false", "no", "off")


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


def step_is_cache(step: dict) -> bool:
    uses = step_value(step, "uses") or ""
    return any(uses.startswith(a) for a in CACHE_ACTIONS)


# --- the shell reader ----------------------------------------------------------------


def segments(body: str) -> list[str]:
    """The simple commands in a run body, as raw text, at command positions only.

    Splits on `;`, `&&`, `||`, `|` and newlines outside quotes, drops `#` comments, and
    recurses into `$(...)`, whose contents really do run. Not a shell: a form it does not
    model simply yields no command, which is the safe direction for a gate check.
    """
    out: list[str] = []
    buf: list[str] = []
    i = 0
    quote = None
    while i < len(body):
        ch = body[i]
        if quote:
            buf.append(ch)
            if ch == "\\" and quote == '"' and i + 1 < len(body):
                buf.append(body[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
            i += 1
            continue
        if ch in "'\"":
            quote = ch
            buf.append(ch)
            i += 1
            continue
        if ch == "$" and i + 1 < len(body) and body[i + 1] == "(":
            j = i + 2
            depth = 1
            while j < len(body) and depth:
                if body[j] == "(":
                    depth += 1
                elif body[j] == ")":
                    depth -= 1
                j += 1
            # What runs is the substitution's own commands; the outer word is not a command,
            # so nothing of it goes back into buf.
            out.extend(segments(body[i + 2:j - 1]))
            i = j
            continue
        if ch == "#" and (not buf or buf[-1].isspace()):
            while i < len(body) and body[i] != "\n":
                i += 1
            continue
        if ch == "\n" or ch in ";&|":
            if ch in ";&|" and i + 1 < len(body) and body[i + 1] == ch:
                i += 1
            if any(c.strip() for c in buf):
                out.append("".join(buf))
            buf = []
            i += 1
            continue
        buf.append(ch)
        i += 1
    if any(c.strip() for c in buf):
        out.append("".join(buf))
    return out


def tokens(segment: str) -> list[str]:
    """Words of one simple command, with quotes removed and redirections dropped."""
    out: list[str] = []
    buf: list[str] = []
    i = 0
    quote = None
    while i < len(segment):
        ch = segment[i]
        if quote:
            if ch == "\\" and quote == '"' and i + 1 < len(segment):
                buf.append(segment[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
            else:
                buf.append(ch)
            i += 1
            continue
        if ch in "'\"":
            quote = ch
            i += 1
            continue
        if ch.isspace():
            if buf:
                out.append("".join(buf))
                buf = []
            i += 1
            continue
        if ch in "<>":
            # An fd number in front of the operator is not a word (`2>&1`).
            if buf and "".join(buf).isdigit():
                buf = []
            elif buf:
                out.append("".join(buf))
                buf = []
            i += 1
            while i < len(segment) and segment[i] in "&0123456789":
                i += 1
            while i < len(segment) and not segment[i].isspace():
                i += 1
            continue
        buf.append(ch)
        i += 1
    if buf:
        out.append("".join(buf))
    return out


def command(segment: str) -> list[str]:
    """One segment's words with the leading assignments and wrappers this file uses removed."""
    t = tokens(segment)
    while t:
        head = t[0]
        if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", head):
            t = t[1:]
            continue
        if head in ("sudo", "env", "!", "command", "exec", "nohup"):
            t = t[1:]
            continue
        if head == "timeout":
            t = t[1:]
            while t and (t[0].startswith("-") or re.fullmatch(r"\d+(?:\.\d+)?[smhd]?", t[0])):
                t = t[1:]
            continue
        break
    return t


def step_runs(step: dict, cmd: str) -> bool:
    """Whether this step runs `cmd` at a command position, not merely mentions it."""
    want = cmd.split()
    for seg in segments(step_body(step)):
        got = command(seg)
        if got[:len(want)] == want:
            return True
    return False


# --- the check -----------------------------------------------------------------------


def check(text: str) -> list[str]:
    """Every problem, as a line of text; empty when the workflow is as required."""
    problems = []
    jobs = scan(text)
    for job, cmd in EXPECTED:
        j = jobs.get(job)
        if j is None:
            problems.append(f"{job}: job is missing but runs required work")
            continue
        hits = [s for s in j["steps"] if step_runs(s, cmd)]
        if not hits:
            problems.append(f"{job}: required command {cmd!r} is missing or only mentioned, not run")
            continue
        for s in hits:
            name = step_name(s)
            if step_value(s, "if") is not None:
                problems.append(f"{job}: required step {name!r} ({cmd}) is conditional")
            if truthy(step_value(s, "continue-on-error")):
                problems.append(f"{job}: required step {name!r} ({cmd}) may fail")
        if j["if"]:
            problems.append(f"{job}: the job is conditional but runs required work")
        if j["continue"] and job not in EXEMPT:
            problems.append(f"{job}: the job may fail but runs required work")
    for job, j in jobs.items():
        for s in j["steps"]:
            uses = (step_value(s, "uses") or "").strip("\"'")
            action = uses.split("@", 1)[0]
            if any(action == name or action.startswith(name + "/") for name in ACTIONS_CACHE_ACTIONS):
                problems.append(f"{job}: {uses} spends the fork's Actions-cache budget outside the bundle producer")
            if action.endswith("/.github/actions/toolchain") and (step_value(s, "cache") or "").strip("\"' "):
                # A bundle consumer restores the producer's entry read-only: `cache-save` must be
                # literally false. Anything else (absent, true, an expression) could write.
                save = (step_value(s, "cache-save") or "").strip().strip("\"'").lower()
                if save != "false":
                    problems.append(f"{job}: toolchain cache allocation belongs to the bundle producer (cache-save is {save or 'unset'!r}, not false)")
            if not step_is_cache(s) and truthy(step_value(s, "continue-on-error")):
                problems.append(f"{job}: {step_name(s)!r} carries continue-on-error but is not a cache step")
    for job, j in jobs.items():
        for s in j["steps"]:
            if not step_is_cache(s):
                continue
            gate = step_value(s, "if")
            if gate is None:
                problems.append(f"{job}: the cache step is not gated on ENABLE_FLAKEHUB_CACHE")
            elif normalize_expr(gate) not in OPT_IN:
                problems.append(
                    f"{job}: the cache step's if: is {gate!r}, not the supported opt-in predicate")
            coe = step_value(s, "continue-on-error")
            if (coe or "").strip().strip("\"'").lower() != "true":
                problems.append(f"{job}: the cache step's continue-on-error is {coe!r}, not literally true")
    return problems


def normalize_expr(expr: str) -> str:
    """An `if:` expression with `${{ }}` and runs of spaces removed, for exact comparison."""
    v = expr.strip()
    if v.startswith("${{") and v.endswith("}}"):
        v = v[3:-2].strip()
    return re.sub(r"\s+", " ", v)


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
    print(f"{path}: the cache is optional; every required command runs at a command position")
    return 0


if __name__ == "__main__":
    sys.exit(main())
