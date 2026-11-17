"""The benchmark result contract shared by measurement, publication and history readers.

Schema 2 records Blaze alone. Unversioned files are historical comparisons; their Cargo
fields retain their original meaning. Reading either format never rewrites the source file.
"""

import json
import math
import os
import re
from pathlib import PureWindowsPath

VERSION = 2


def number(value):
    return (isinstance(value, (int, float)) and not isinstance(value, bool) and value >= 0
            and (not isinstance(value, float) or math.isfinite(value)))


def native(result):
    return isinstance(result, dict) and type(result.get("schema")) is int and result["schema"] == VERSION


def load_input(text, choices, default, script):
    """One strict frozen action dataclass, from inline JSON, a path, or standard input."""
    from typed_input import InputError, fail, parse, read
    try:
        value = read(text)
        action = value.get("action", default) if isinstance(value, dict) else None
        if not isinstance(action, str) or action not in choices:
            raise ValueError(f"action must be one of {', '.join(choices)}")
        return parse(choices[action], json.dumps(value), script)
    except (InputError, OSError, ValueError) as error:
        fail(script, str(error))


def measured(result):
    rows = dict(result.get("results") or {})
    rows.update((f"stress:{key}", value) for key, value in (result.get("stress") or {}).items())
    return rows


def errors(result):
    """Refusals that make a result unusable as a completed measurement, in either schema."""
    if not isinstance(result, dict):
        return ["result is not an object"]
    if "schema" in result and not native(result):
        return [f"unsupported benchmark schema {result['schema']!r}"]
    wrong = []
    for field in ("results", "stress"):
        if field == "results" or field in result:
            if not isinstance(result.get(field), dict):
                wrong.append(f"{field} is not an object")
    if wrong:
        return wrong
    rows = measured(result)
    if not rows:
        wrong.append("benchmark measured nothing")
    if result.get("stress_gap"):
        wrong.append(f"stress: {result['stress_gap']}")
    if native(result):
        if not isinstance(result.get("date"), str) or not re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", result["date"]):
            wrong.append("date must identify the measurement in UTC")
        if not isinstance(result.get("toolchain"), str) or not result["toolchain"]:
            wrong.append("the selected Blaze toolchain is not identified")
        if type(result.get("runs")) is not int or result["runs"] < 1:
            wrong.append("runs must be a positive count")
        identity = result.get("compiler")
        if not isinstance(identity, dict):
            wrong.append("compiler identity is missing")
        else:
            path, digest, binaries = identity.get("path"), identity.get("sha256"), identity.get("binaries")
            if not isinstance(path, str) or not (os.path.isabs(path) or PureWindowsPath(path).is_absolute()):
                wrong.append("compiler.path is not absolute")
            if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
                wrong.append("compiler.sha256 is not a SHA-256 digest")
            if not isinstance(path, str) or not isinstance(binaries, dict) or not binaries or binaries.get(path) != digest:
                wrong.append("compiler.binaries does not identify the selected executable")
            elif any(not isinstance(p, str) or not (os.path.isabs(p) or PureWindowsPath(p).is_absolute())
                     or not isinstance(h, str) or not re.fullmatch(r"[0-9a-f]{64}", h) for p, h in binaries.items()):
                wrong.append("compiler.binaries contains an invalid path or SHA-256 digest")
            if result.get("compiler_after") != identity:
                wrong.append("compiler identity was not verified unchanged after the run")
        if result.get("setups"):
            wrong.append("schema 2 cannot contain Cargo setups")
    for key, row in rows.items():
        if not isinstance(row, dict):
            wrong.append(f"{key}: measurement is not an object")
            continue
        if row.get("failed"):
            wrong.append(f"{key}: {row['failed']}")
        issues = row.get("wrong", [])
        if not isinstance(issues, list) or any(not isinstance(issue, str) for issue in issues):
            wrong.append(f"{key}: wrong must be a list of findings")
        else:
            wrong.extend(f"{key}: {issue}" for issue in issues)
        phases = row.get("phases")
        if not isinstance(phases, dict) or not phases:
            wrong.append(f"{key}: no phases measured")
            continue
        if native(result):
            expected = {"check", "build"} if key.startswith("stress:") else {"cold", "no-op"}
            for absent in sorted(expected - phases.keys()):
                wrong.append(f"{key}: {absent} not measured")
        for label, phase in phases.items():
            name = f"{key} {label}"
            if not isinstance(phase, dict):
                wrong.append(f"{name}: phase is not an object")
                continue
            if not number(phase.get("wall_s")):
                wrong.append(f"{name}: wall_s is missing or not finite and nonnegative")
            if label == "test" and (phase.get("same") is False or phase.get("difference") is not None):
                wrong.append(f"{name}: stored test verdict failed")
            for field in ("rss_mb", "compiled", "build_s", "warm_s", "back_s", "edit_s", "instructions", "cycles"):
                if phase.get(field) is not None and not number(phase[field]):
                    wrong.append(f"{name}: {field} is not finite and nonnegative")
            memory = phase.get("memory")
            if memory is not None and not isinstance(memory, dict):
                wrong.append(f"{name}: memory is not an object")
                memory = None
            if isinstance(memory, dict):
                for side, values in memory.items():
                    if not isinstance(values, dict) or not number(values.get("peak_mb")):
                        wrong.append(f"{name}: {side} memory has no valid peak_mb")
            if native(result):
                if phase.get("built") != "daemon":
                    wrong.append(f"{name}: not built by the selected Blaze daemon")
                samples = phase.get("samples_s")
                if not isinstance(samples, list) or not samples or not all(map(number, samples)):
                    wrong.append(f"{name}: no valid timing samples")
                if any("cargo" in field or field == "setups" for field in phase):
                    wrong.append(f"{name}: schema 2 cannot contain Cargo comparison fields")
                if isinstance(memory, dict) and "cargo" in memory:
                    wrong.append(f"{name}: schema 2 cannot contain Cargo memory")
                if label == "test":
                    if phase.get("same") is not True or phase.get("difference") is not None:
                        wrong.append(f"{name}: the project's test verdict did not pass")
                    if not number(phase.get("edit_s")):
                        wrong.append(f"{name}: edit_s is missing or invalid")
                    for field in ("first_s", "edit_samples_s"):
                        values = phase.get(field)
                        if not isinstance(values, list) or not values or not all(map(number, values)):
                            wrong.append(f"{name}: {field} has no valid timing samples")
                    if not number(phase.get("tests")) or phase["tests"] < 1:
                        wrong.append(f"{name}: no tests ran")
                else:
                    if not isinstance(memory, dict) or not isinstance(memory.get("blaze"), dict):
                        wrong.append(f"{name}: Blaze memory was not measured")
                    if not key.startswith("stress:"):
                        for field in ("compiled", "rss_mb"):
                            if not number(phase.get(field)):
                                wrong.append(f"{name}: {field} was not measured")
                        if label not in ("cold", "no-op"):
                            if not number(phase.get("back_s")):
                                wrong.append(f"{name}: reversal time was not measured")
                            for field in ("link", "back_link"):
                                if phase.get(field) not in ("spliced", "first link"):
                                    wrong.append(f"{name}: {field} does not establish a retained or new image")
        runtime = row.get("runtime")
        if runtime is not None and not isinstance(runtime, dict):
            wrong.append(f"{key}: runtime is not an object")
            continue
        if isinstance(runtime, dict) and "blaze" in runtime:
            if not isinstance(runtime["blaze"], dict) or not number(runtime["blaze"].get("median_s")):
                wrong.append(f"{key}: runtime has no valid Blaze time")
        if native(result) and isinstance(runtime, dict):
            if set(runtime) - {"command", "blaze"}:
                wrong.append(f"{key}: schema 2 runtime measures only Blaze")
            if not isinstance(runtime.get("blaze"), dict):
                wrong.append(f"{key}: runtime has no valid Blaze time")
            else:
                for field in ("median_s", "min_s", "max_s", "runs", "peak_mb", "peak_max_mb", "size_mb"):
                    if not number(runtime["blaze"].get(field)):
                        wrong.append(f"{key}: Blaze runtime {field} is missing or invalid")
            if not isinstance(runtime.get("command"), list) or not runtime["command"] or any(not isinstance(arg, str) for arg in runtime["command"]):
                wrong.append(f"{key}: runtime command is not identified")
    return wrong
