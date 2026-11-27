#!/usr/bin/env python3
"""What agents have learned (rule agents-json): one file per row under `agents/`.

    python3 -I harness/v2/agents.py '<one JSON value>'

    {"howto": true}                      every how-to row, in full (read these before starting)
    {"find": "watcher"}                  annoyance rows whose text mentions the words (all of them), newest first, one line each
    {"find": "lock", "full": true}       the same rows in full
    {"show": "reth-1112-3"}              one row in full
    {"classes": true}                    the open classes of problem, with how many rows name each
    {"add": {"kind": "annoyance", "lane": "reth-1112", "row": {...}}}
                                         writes agents/annoyances/<lane>-<n>.json with the next free n and prints its id
    {"add": {"kind": "howto", "name": "build-blaze", "row": {...}}}
                                         writes agents/howto/<name>.json (refuses to overwrite: supersede by a new name)
    {"import": "path/to/agents.json", "lane": "old"}
                                         rows of the old one-file layout not yet present are written as files
    {"check": true}                      every file parses, has its fields, and its id is its file name

A row's id is its file name, so two lanes can never take one id and no merge touches another's row.
"""
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import cast

ROOT = Path(__file__).resolve().parents[2] / "agents"
KINDS = {"howto": ("id", "date", "who", "task", "do", "why", "state"), "annoyances": ("id", "date", "who", "where", "what", "state")}


@dataclass(frozen=True)
class Ask:
    howto: bool = False
    find: str | None = None
    full: bool = False
    show: str | None = None
    classes: bool = False
    add: dict[str, object] | None = None
    check: bool = False
    imported: str | None = None
    lane: str | None = None


def ask() -> Ask:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    value: object = json.loads(sys.argv[1])
    if not isinstance(value, dict):
        sys.exit("agents.py: the input is one JSON object")
    raw = dict(cast(dict[str, object], value))
    if "import" in raw:
        raw["imported"] = raw.pop("import")
    unknown = set(raw) - set(Ask.__dataclass_fields__)
    if unknown:
        sys.exit(f"agents.py: unknown field {sorted(unknown)}")
    flags: dict[str, bool] = {}
    for name in ("howto", "full", "classes", "check"):
        flag = raw.get(name, False)
        if not isinstance(flag, bool):
            sys.exit(f"agents.py: {name} is a boolean")
        flags[name] = flag
    strings: dict[str, str | None] = {}
    for name in ("find", "show", "imported", "lane"):
        string = raw.get(name)
        if string is not None and not isinstance(string, str):
            sys.exit(f"agents.py: {name} is a string or null")
        strings[name] = string
    add = raw.get("add")
    if add is not None and not isinstance(add, dict):
        sys.exit("agents.py: add is a JSON object or null")
    return Ask(
        howto=flags["howto"], find=strings["find"], full=flags["full"], show=strings["show"],
        classes=flags["classes"], add=cast(dict[str, object] | None, add), check=flags["check"],
        imported=strings["imported"], lane=strings["lane"],
    )


def object_row(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise TypeError(f"agents.py: {where} is a JSON object")
    return cast(dict[str, object], value)


def rows(kind: str) -> list[dict[str, object]]:
    out: list[dict[str, object]] = []
    for path in sorted((ROOT / kind).glob("*.json")):
        row = object_row(json.loads(path.read_text()), str(path))
        row["id"] = path.stem
        out.append(row)
    return out


def write(kind: str, name: str, row: dict[str, object]) -> str:
    safe = "".join(c if c.isalnum() or c in "-_." else "-" for c in name).strip("-.")
    if not safe:
        sys.exit(f"agents.py: `{name}` names no file")
    path = ROOT / kind / f"{safe}.json"
    if path.exists():
        sys.exit(f"agents.py: {path} exists: a row is never overwritten; add one under a new name and set the old one's state")
    row = {"id": safe, **{k: v for k, v in row.items() if k != "id"}}
    missing = [f for f in KINDS[kind] if f not in row]
    if missing:
        sys.exit(f"agents.py: the row lacks {missing}")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(row, indent=1, ensure_ascii=False) + "\n")
    return safe


def text(row: dict[str, object]) -> str:
    return json.dumps(row, ensure_ascii=False).lower()


def row_string(row: dict[str, object], name: str) -> str:
    value = row.get(name, "")
    if not isinstance(value, str):
        raise TypeError(f"agents.py: row {row.get('id')}: {name} is a string")
    return value


def one_line(row: dict[str, object]) -> str:
    what = " ".join(str(row.get("what") or row.get("task") or "").split())
    return f"{row_string(row, 'id'):22s} {row_string(row, 'date'):10s} {row_string(row, 'state'):8.8s} {row_string(row, 'where')[:28]:28s} {what[:110]}"


def main() -> None:
    a = ask()
    if a.howto:
        for row in rows("howto"):
            if str(row.get("state", "")).startswith("current"):
                print(json.dumps(row, indent=1, ensure_ascii=False))
    elif a.find is not None:
        words = a.find.lower().split()
        hits = [r for r in rows("annoyances") + rows("howto") if all(w in text(r) for w in words)]
        hits.sort(key=lambda r: str(r.get("date", "")), reverse=True)
        for row in hits:
            print(json.dumps(row, indent=1, ensure_ascii=False) if a.full else one_line(row))
        print(f"{len(hits)} rows", file=sys.stderr)
    elif a.show is not None:
        for kind in KINDS:
            path = ROOT / kind / f"{a.show}.json"
            if path.exists():
                print(path.read_text(), end="")
                return
        sys.exit(f"agents.py: no row `{a.show}`")
    elif a.classes:
        count: dict[str, list[str]] = {}
        for row in rows("annoyances"):
            if row.get("class") and row.get("state") != "fixed":
                count.setdefault(" ".join(str(row["class"]).split())[:160], []).append(row_string(row, "id"))
        for name, ids in sorted(count.items(), key=lambda kv: -len(kv[1])):
            print(f"{len(ids):3d}  {name}  [{', '.join(ids[:6])}]")
    elif a.add is not None:
        kind = {"annoyance": "annoyances", "howto": "howto"}.get(str(a.add.get("kind")))
        added_row = a.add.get("row")
        if kind is None or not isinstance(added_row, dict):
            sys.exit("agents.py: add takes kind (annoyance or howto) and row")
        row = cast(dict[str, object], added_row)
        if kind == "howto":
            print(write(kind, str(a.add.get("name", "")), row))
        else:
            lane = str(a.add.get("lane", ""))
            if not lane:
                sys.exit("agents.py: an annoyance names its lane (`main` for the main agent)")
            n = 1
            while (ROOT / kind / f"{lane}-{n}.json").exists():
                n += 1
            print(write(kind, f"{lane}-{n}", row))
    elif a.imported is not None:
        old = object_row(json.loads(Path(a.imported).read_text()), a.imported)
        have = {kind: {json.dumps({k: v for k, v in r.items() if k != "id"}, sort_keys=True) for r in rows(kind)} for kind in KINDS}
        made = 0
        for kind in KINDS:
            imported_rows = old.get(kind, [])
            if not isinstance(imported_rows, list):
                sys.exit(f"agents.py: {a.imported}: {kind} is a list of rows")
            for imported_row in cast(list[object], imported_rows):
                row = object_row(imported_row, f"{a.imported}: {kind} row")
                body = json.dumps({k: v for k, v in row.items() if k != "id"}, sort_keys=True)
                if body in have[kind]:
                    continue
                name = str(row.get("id", "row"))
                if (ROOT / kind / f"{name}.json").exists():
                    name = f"{a.lane or 'old'}-{name}"
                n = 1
                base = name
                while (ROOT / kind / f"{name}.json").exists():
                    n += 1
                    name = f"{base}-{n}"
                write(kind, name, row)
                made += 1
        print(f"{made} rows written")
    elif a.check:
        bad = 0
        for kind, fields in KINDS.items():
            for path in sorted((ROOT / kind).glob("*.json")):
                try:
                    row = object_row(json.loads(path.read_text()), str(path))
                except (ValueError, TypeError) as error:
                    print(f"{path}: {error}")
                    bad += 1
                    continue
                missing = [f for f in fields if f not in row]
                if missing or row.get("id") != path.stem:
                    print(f"{path}: lacks {missing}" if missing else f"{path}: its id is `{row.get('id')}`, not its file name")
                    bad += 1
        total = sum(len(list((ROOT / k).glob("*.json"))) for k in KINDS)
        if total == 0:
            sys.exit("agents.py: no row found under agents/: the check ran on nothing")
        print(f"{total} rows, {bad} bad")
        sys.exit(1 if bad else 0)
    else:
        sys.exit(__doc__)


main()
