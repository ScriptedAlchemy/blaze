#!/usr/bin/env python3
"""Check GitHub-hosted runner policy in every workflow.

    PYTHONPATH=tools/typed-input/src python3 .github/scripts/check_runner_labels.py '{}'

Ported from main's runner-safety check (#63), under V2's workflows gate.
Literal runners must be hosted, including matrices and reusable-workflow callers.
Pull-request and merge-group workflows must resolve their runner expressions from
literal matrices. Trusted non-PR workflows can use runtime matrices and inputs;
benchmark labels come from harness/benchplatforms.py. Repository runner access and
fork-approval settings still require an operator's check outside this script.

The small YAML reader below is inherited from main. Unsupported syntax fails the
check. The workflows job in blaze.yaml runs this and test_check_runner_labels.py.
"""
import glob, os, re, sys
from dataclasses import dataclass
from typed_input import parse

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
WORKFLOWS = os.path.join(ROOT, ".github", "workflows")
PR_EVENTS = ("pull_request", "pull_request_target", "merge_group")
# Hosted runners, and only them: `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-24.04-arm`,
# `macos-15`, `windows-2025`, ...
HOSTED = re.compile(r"^(ubuntu|windows|macos)-[a-z0-9][a-z0-9.-]*$")
EXPRESSION = re.compile(r"^\$\{\{(.*)\}\}$", re.S)
MATRIX_REF = re.compile(r"^matrix\.([A-Za-z_][A-Za-z0-9_-]*)$")
# The `with:` inputs of a reusable workflow that become a `runs-on` on the other side.
RUNNER_INPUTS = ("os", "runner", "runners", "runs-on", "labels")


class Unreadable(Exception):
    """The workflow uses YAML this checker does not read: the check fails rather than guess."""


# --- the YAML the workflows are written in -----------------------------------------------------
# A reader for block mappings and sequences, flow sequences, quoted and plain scalars and block
# scalars (`|`, `>`, with the usual indicators): what a GitHub workflow uses, and nothing else.
# Anything beyond it raises Unreadable, which fails the check.


def _strip_comment(line):
    """The line without its comment, `#` only counting where YAML starts one."""
    out, quote = [], None
    for i, ch in enumerate(line):
        if quote:
            out.append(ch)
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(ch)
        elif ch == "#" and (i == 0 or line[i - 1] in " \t"):
            break
        else:
            out.append(ch)
    return "".join(out).rstrip()


def _lines(text):
    lines = []
    for number, raw in enumerate(text.splitlines(), 1):
        body = _strip_comment(raw)
        if not body.strip():
            continue
        indent = len(body) - len(body.lstrip(" "))
        if body[indent] == "\t":
            raise Unreadable(f"line {number}: a tab is not indentation")
        lines.append((indent, body[indent:], number))
    return lines


def _unquote(text, number):
    if text[:1] in "\"'":
        if len(text) < 2 or text[-1] != text[0]:
            raise Unreadable(f"line {number}: unterminated quoted string {text}")
        return text[1:-1]
    return text


def _key_value(text, number):
    """`key: value` as (key, value), or None when the text is not one."""
    quote, depth = None, 0
    for i, ch in enumerate(text):
        if quote:
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch in "[{":
            depth += 1
        elif ch in "]}":
            depth -= 1
        elif ch == ":" and depth == 0 and (i + 1 == len(text) or text[i + 1] in " \t"):
            return _unquote(text[:i].strip(), number), text[i + 1 :].strip()
    return None


def _flow_parts(text, number):
    """The comma-separated items inside `[...]` or `{...}`, at the top level."""
    if not text.endswith("]") and not text.endswith("}"):
        raise Unreadable(f"line {number}: {text!r} is not a flow collection on one line")
    parts, current, quote, depth = [], [], None, 0
    for ch in text[1:-1]:
        if quote:
            current.append(ch)
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            current.append(ch)
        elif ch in "[{":
            depth += 1
            current.append(ch)
        elif ch in "]}":
            depth -= 1
            current.append(ch)
        elif ch == "," and depth == 0:
            parts.append("".join(current).strip())
            current = []
        else:
            current.append(ch)
    last = "".join(current).strip()
    if last:
        parts.append(last)
    return parts


def _flow_sequence(text, number):
    return [_scalar(part, number) for part in _flow_parts(text, number)]


def _flow_mapping(text, number):
    mapping = {}
    for part in _flow_parts(text, number):
        pair = _key_value(part, number)
        if pair is None:
            raise Unreadable(f"line {number}: {part!r} in {text} is not `key: value`")
        mapping[pair[0]] = _scalar(pair[1], number)
    return mapping


def _scalar(text, number):
    if text.startswith("["):
        return _flow_sequence(text, number)
    if text.startswith("{"):
        return _flow_mapping(text, number)
    if text[:1] in "\"'":
        return _unquote(text, number)
    return text


def _is_item(text):
    return text == "-" or text.startswith("- ")


def _mapping(lines, i, indent):
    mapping = {}
    while i < len(lines) and lines[i][0] == indent and not _is_item(lines[i][1]):
        number = lines[i][2]
        pair = _key_value(lines[i][1], number)
        if pair is None:
            raise Unreadable(f"line {number}: {lines[i][1]!r} is not `key: value` (a plain scalar over several lines?)")
        key, rest = pair
        i += 1
        if not rest:
            if i < len(lines) and lines[i][0] > indent:
                value, i = _block(lines, i, lines[i][0])
            else:
                value = None
        elif re.fullmatch(r"[|>][-+]?[0-9]?", rest):
            # A block scalar: its lines are content, more indented than the key.
            while i < len(lines) and lines[i][0] > indent:
                i += 1
            value = None
        else:
            value = _scalar(rest, number)
        mapping[key] = value
    return mapping, i


def _sequence(lines, i, indent):
    items = []
    while i < len(lines) and lines[i][0] == indent and _is_item(lines[i][1]):
        number = lines[i][2]
        rest = lines[i][1][1:].strip()
        if not rest:
            i += 1
            if i < len(lines) and lines[i][0] > indent:
                value, i = _block(lines, i, lines[i][0])
            else:
                value = None
        elif _key_value(rest, number) is not None:
            # `- key: value`: the item is a mapping two columns in from the dash, which is where
            # its other keys are too.
            lines[i] = (indent + 2, rest, number)
            value, i = _mapping(lines, i, indent + 2)
        else:
            value = _scalar(rest, number)
            i += 1
        items.append(value)
    return items, i


def _block(lines, i, indent):
    return _sequence(lines, i, indent) if _is_item(lines[i][1]) else _mapping(lines, i, indent)


def read(text):
    """The workflow a YAML text describes, as dicts, lists and strings."""
    lines = _lines(text)
    if not lines:
        raise Unreadable("the workflow is empty")
    value, i = _block(lines, 0, lines[0][0])
    if i != len(lines):
        raise Unreadable(f"line {lines[i][2]}: {lines[i][1]!r} is not part of the workflow")
    if not isinstance(value, dict):
        raise Unreadable("the workflow is not a mapping")
    return value


# --- the check ---------------------------------------------------------------------------------


def _events(workflow):
    on = workflow.get("on")
    if on is None:
        return []
    if isinstance(on, str):
        return [on]
    if isinstance(on, list):
        return [str(event) for event in on]
    if isinstance(on, dict):
        return list(on)
    raise Unreadable("`on:` is not an event, a list of them or a mapping of them")


def _matrix_values(job, key):
    """The values `matrix.<key>` takes in this job, from its strategy, `include` and all."""
    strategy = job.get("strategy")
    if strategy is None:
        return []
    if not isinstance(strategy, dict):
        raise Unreadable(f"`strategy:` is not a mapping: {strategy!r}")
    matrix = strategy.get("matrix")
    if matrix is None:
        return []
    if not isinstance(matrix, dict):
        return []  # `${{ fromJSON(...) }}`: not resolvable here, and the caller says so
    values = []
    if key in matrix:
        value = matrix[key]
        values.extend(value if isinstance(value, list) else [value])
    include = matrix.get("include")
    if isinstance(include, list):
        for entry in include:
            if isinstance(entry, dict) and key in entry:
                values.append(entry[key])
    return values


def _label_problems(path, job_id, where, label, job, pull_request):
    if not isinstance(label, str):
        return [f"{path}: job {job_id}: {where} is not a label this checker reads: {label!r}"]
    expression = EXPRESSION.match(label.strip())
    if expression:
        reference = MATRIX_REF.match(expression.group(1).strip())
        if not pull_request and (reference is None or not _matrix_values(job, reference.group(1))):
            return []  # trusted workflows may use runtime matrices or workflow_call inputs
        if reference is None:
            return [
                f"{path}: job {job_id}: {where} is {label!r}, which this checker cannot resolve to "
                f"hosted runners; a workflow a pull request runs may only name ubuntu-*, windows-* "
                f"or macos-*, or a matrix of them"
            ]
        values = _matrix_values(job, reference.group(1))
        if not values:
            return [
                f"{path}: job {job_id}: {where} is {label!r}, but no `strategy.matrix` of the job "
                f"gives matrix.{reference.group(1)} a value this checker can read"
            ]
        return [
            problem
            for value in values
            for problem in _label_problems(path, job_id, where, value, job, pull_request=True)
        ]
    for unusual, why in (
        (label == "self-hosted", "a bare self-hosted label matches every self-hosted runner of the repository"),
        (label.endswith("-runner"), "a *-runner label names a machine of someone's"),
        (label.startswith("bench-"), "a bench-* label names a benchmark machine"),
    ):
        if unusual:
            return [f"{path}: job {job_id}: {where} is {label!r}: {why}, which no job may ask for"]
    if HOSTED.fullmatch(label):
        return []
    return [
        f"{path}: job {job_id}: {where} is {label!r}, which is not a GitHub-hosted runner; "
        f"all workflows must use hosted capacity"
    ]


def check(path, text):
    """Every problem in one workflow's text, as messages; Unreadable if it cannot be read."""
    workflow = read(text)
    pull_request = any(event in PR_EVENTS for event in _events(workflow))
    jobs = workflow.get("jobs")
    if jobs is None:
        return []
    if not isinstance(jobs, dict):
        raise Unreadable("`jobs:` is not a mapping")
    problems = []
    for job_id, job in jobs.items():
        if job is None:
            continue
        if not isinstance(job, dict):
            raise Unreadable(f"job {job_id} is not a mapping")
        runs = job.get("runs-on")
        if runs is not None:
            for label in runs if isinstance(runs, list) else [runs]:
                problems += _label_problems(path, job_id, "runs-on", label, job, pull_request)
        with_ = job.get("with")
        if isinstance(with_, dict):
            for key, value in with_.items():
                if key in RUNNER_INPUTS:
                    for label in value if isinstance(value, list) else [value]:
                        problems += _label_problems(path, job_id, f"with.{key}", label, job, pull_request)
    return problems


@dataclass(frozen=True)
class Input:
    root: str = ROOT


def main():
    if len(sys.argv) != 2:
        raise SystemExit("check_runner_labels.py: expected one JSON input, e.g. '{}'")
    given = parse(Input, sys.argv[1], "check_runner_labels.py")
    workflows = os.path.join(given.root, ".github", "workflows")
    paths = sorted(glob.glob(os.path.join(workflows, "*.yaml")) + glob.glob(os.path.join(workflows, "*.yml")))
    if not paths:
        raise SystemExit(f"check_runner_labels: no workflows in {workflows}")
    problems = []
    for path in paths:
        with open(path, encoding="utf-8") as f:
            text = f.read()
        try:
            problems += check(path, text)
        except Unreadable as error:
            problems.append(f"{path}: cannot be checked: {error}")
    annotation = os.environ.get("GITHUB_ACTIONS") == "true"
    for problem in problems:
        print(f"::error::{problem}" if annotation else problem)
    if problems:
        sys.exit(1)
    print(f"check_runner_labels: {len(paths)} workflows, no self-hosted runner a pull request can pick")


if __name__ == "__main__":
    main()
