#!/usr/bin/env python3
"""fetch.py: the fixtures of the distributed program, at the commits they are pinned to.

    python3 harness/dist/fetch.py            every fixture, into ~/work/<name>
    python3 harness/dist/fetch.py --list     what is pinned, and where it is
    python3 harness/dist/fetch.py NAME...    only those
    python3 harness/dist/fetch.py --dir DIR  fetch below DIR instead of ~/work
    python3 harness/dist/fetch.py --offline  refuse to download: fail if a fixture is not there

`harness/dist/fixtures.toml` pins them (docs/distributed.md). A `repo` fixture is cloned and
checked out at its full commit id; a `project` fixture is written from the files it names. A
fixture that is already at its commit is left alone. The paths are what the harnesses take as
`--td`: `python3 -I harness/tracedecay.py --td ~/work/hello --predicate P0-A`.

The commit is checked, not assumed: after the checkout, `git rev-parse HEAD` has to be the pinned
id, or fetch.py fails and says which commit it found. A branch or a tag is never fetched, so what
a number was measured on cannot move under it.

Python 3 standard library only. It reads the TOML subset fixtures.toml uses (tables, arrays of
tables, strings, multi-line strings, arrays of strings, booleans) and refuses anything else, rather
than guess: Python before 3.11 has no TOML reader in its standard library, and the harnesses run
on the interpreter the machine has.
"""
import argparse, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "fixtures.toml")
DEFAULT_DIR = os.path.join(os.path.expanduser("~"), "work")


class TomlError(Exception):
    pass


def parse_toml(text):
    """The subset of TOML fixtures.toml uses: tables, arrays of tables, keys, basic and
    triple-quoted strings, arrays of strings, integers and booleans. Anything else raises: a
    fixture pinned in a syntax this does not read would be a fixture nobody could check."""
    root, path = {}, []

    def where(i):
        return "line %d" % (text.count("\n", 0, i) + 1)

    def skip_space(i):
        while i < len(text) and text[i] in " \t\r\n":
            i += 1
        return i

    def skip_line(i):
        """Past the end of the line: a comment, or nothing."""
        i = skip_space(i)
        if i < len(text) and text[i] == "#":
            while i < len(text) and text[i] != "\n":
                i += 1
        return i

    def string_at(i):
        if text.startswith('"""', i):
            end = text.find('"""', i + 3)
            if end < 0:
                raise TomlError("an unterminated \"\"\" string at " + where(i))
            return text[i + 3:end], end + 3
        if i >= len(text) or text[i] != '"':
            raise TomlError(f"a value at {where(i)} is not a string: {text[i:i + 20]!r}")
        out, j = [], i + 1
        while j < len(text):
            if text[j] == "\\":
                esc = {"n": "\n", "t": "\t", '"': '"', "\\": "\\", "r": "\r"}.get(text[j + 1])
                if esc is None:
                    raise TomlError(f"the escape {text[j:j + 2]!r} at {where(j)} is not one this reader knows")
                out.append(esc)
                j += 2
                continue
            if text[j] == '"':
                return "".join(out), j + 1
            out.append(text[j])
            j += 1
        raise TomlError("an unterminated string at " + where(i))

    def value_at(i):
        i = skip_space(i)
        if text.startswith('"""', i) or (i < len(text) and text[i] == '"'):
            return string_at(i)
        if i < len(text) and text[i] == "[":
            out, j = [], i + 1
            while True:
                j = skip_space(j)
                if j < len(text) and text[j] == "]":
                    return out, j + 1
                if text.startswith('"""', j) or (j < len(text) and text[j] == '"'):
                    item, j = string_at(j)
                    out.append(item)
                elif j < len(text) and text[j] in "0123456789-+":
                    m = re.match(r"[-+]?\d+(?:\.\d+)?", text[j:])
                    out.append(float(m.group(0)) if "." in m.group(0) else int(m.group(0)))
                    j += m.end()
                elif text.startswith("true", j) or text.startswith("false", j):
                    out.append(text.startswith("true", j))
                    j += 4 if text.startswith("true", j) else 5
                else:
                    raise TomlError(f"an array item at {where(j)} is not a string, a number or a boolean: {text[j:j + 20]!r}")
                j = skip_space(j)
                if j < len(text) and text[j] == ",":
                    j += 1
                    continue
                if j < len(text) and text[j] == "]":
                    return out, j + 1
                raise TomlError(f"an array at {where(j)} has no `,` or `]` after its item")
        if text.startswith("true", i):
            return True, i + 4
        if text.startswith("false", i):
            return False, i + 5
        raise TomlError(f"a value at {where(i)} is not a string, an array or a boolean: {text[i:i + 20]!r}")

    def node_for(parts):
        """The table at `parts`, made if it is not there. A part that names a table which is an
        array of tables is its last table."""
        node = root
        for part in parts:
            got = node.setdefault(part, {})
            node = got[-1] if isinstance(got, list) else got
        return node

    i = 0
    table = root
    while True:
        i = skip_space(i)
        if i >= len(text):
            return root
        if text[i] == "#":
            i = skip_line(i)
            continue
        if text[i] == "[":
            m = re.match(r"\[\[?([^\[\]]+)\]\]?", text[i:])
            if not m:
                raise TomlError("a table header at " + where(i) + " is not `[name]` or `[[name]]`")
            array = text.startswith("[[", i)
            i += m.end()
            parts = [part.strip() for part in m.group(1).split(".")]
            if array:
                parent = node_for(parts[:-1])
                parent.setdefault(parts[-1], []).append({})
                table = parent[parts[-1]][-1]
            else:
                table = node_for(parts)
            i = skip_line(i)
            continue
        m = re.match(r'("(?:[^"\\]|\\.)*"|[A-Za-z0-9_-]+)\s*=', text[i:])
        if not m:
            raise TomlError(f"a line at {where(i)} is not a table header or `key = value`: {text[i:i + 40]!r}")
        i += m.end()
        got, i = value_at(i)
        table[m.group(1).strip('"')] = got
        while i < len(text) and text[i] in " \t\r":
            i += 1
        if i < len(text) and text[i] not in "#\n":
            raise TomlError(f"trailing text at {where(i)}: {text[i:i + 40]!r}")
        i = skip_line(i)


def fixtures(path=FIXTURES):
    data = parse_toml(open(path).read())
    got = data.get("fixture", [])
    if not got:
        raise SystemExit(f"{path} pins no [[fixture]]")
    names = [it.get("name") for it in got]
    if len(set(names)) != len(names) or None in names:
        raise SystemExit(f"{path} has a fixture without a name, or two with one name: {names}")
    return got


def git(args, cwd=None, check=True):
    ran = subprocess.run(["git"] + args, cwd=cwd, capture_output=True, text=True)
    if check and ran.returncode:
        raise SystemExit(f"git {' '.join(args)} failed ({ran.returncode}): {ran.stderr.strip()}")
    return ran


def head(path):
    return git(["rev-parse", "HEAD"], cwd=path).stdout.strip()


def fetch_repo(fix, dest, offline):
    rev = fix.get("rev", "")
    if not re.fullmatch(r"[0-9a-f]{40}", rev):
        raise SystemExit(f"{fix['name']}: rev {rev!r} is not a full commit id; a fixture is pinned by commit, not a branch or tag")
    if os.path.isdir(os.path.join(dest, ".git")):
        if head(dest) == rev:
            print(f"{fix['name']}: already at {rev[:12]}")
            return
        if offline:
            raise SystemExit(f"{fix['name']}: {dest} is at {head(dest)[:12]}, not {rev[:12]}, and --offline")
        git(["fetch", "origin", rev], cwd=dest)
        git(["checkout", "--detach", rev], cwd=dest)
    else:
        if offline:
            raise SystemExit(f"{fix['name']}: {dest} is not there and --offline")
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        url = "https://github.com/" + fix["repo"] + ".git"
        git(["clone", "--no-checkout", url, dest])
        git(["checkout", "--detach", rev], cwd=dest)
    found = head(dest)
    if found != rev:
        raise SystemExit(f"{fix['name']}: {dest} is at {found}, not the pinned {rev}")
    print(f"{fix['name']}: {dest} at {rev[:12]} ({fix.get('tag') or fix.get('repo')})")


def fetch_project(fix, dest, offline):
    files = fix.get("files") or {}
    if not files:
        raise SystemExit(f"{fix['name']}: a project fixture has no [fixture.files]")
    marker = os.path.join(dest, ".fetch-rev")
    stamp = "\n".join(sorted(f"{name}\n{text}" for name, text in files.items()))
    if os.path.exists(marker) and open(marker).read() == stamp:
        print(f"{fix['name']}: already written")
        return
    for name, text in files.items():
        path = os.path.join(dest, name)
        os.makedirs(os.path.dirname(path) or dest, exist_ok=True)
        with open(path, "w") as f:
            f.write(text)
    with open(marker, "w") as f:
        f.write(stamp)
    print(f"{fix['name']}: {dest} written ({len(files)} files)")


def prepare_note(fix, dest):
    """What the harness has to do to the checkout before it builds it, named by fixtures.toml."""
    if fix.get("prepare") == "tracedecay":
        print(f"{fix['name']}: run `python3 -I harness/tracedecay.py --td {dest} prepare` once "
              "(it moves .pnpm/crates/config.toml aside, clones a CARGO_HOME and patches "
              "recursion_limit)")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("names", nargs="*", help="the fixtures to fetch (default: every one)")
    ap.add_argument("--dir", default=DEFAULT_DIR, help=f"where the fixtures go (default {DEFAULT_DIR})")
    ap.add_argument("--list", action="store_true", help="print what is pinned, and stop")
    ap.add_argument("--offline", action="store_true", help="refuse to download; fail if a fixture is not there")
    a = ap.parse_args()
    pinned = fixtures()
    if a.list:
        for fix in pinned:
            where = os.path.join(a.dir, fix["name"])
            state = "at its commit" if os.path.isdir(where) and (fix["kind"] == "project" or head(where) == fix.get("rev")) else "not fetched"
            print(f"{fix['name']:12} {fix['kind']:8} {fix.get('rev') or 'written from files'}  {where}  ({state})")
            print(f"{'':12} {fix.get('about', '')}")
        return
    chosen = [fix for fix in pinned if not a.names or fix["name"] in a.names]
    missing = sorted(set(a.names) - {fix["name"] for fix in pinned})
    if missing:
        raise SystemExit(f"not pinned in {FIXTURES}: {', '.join(missing)}")
    os.makedirs(a.dir, exist_ok=True)
    for fix in chosen:
        dest = os.path.join(a.dir, fix["name"])
        if fix["kind"] == "repo":
            fetch_repo(fix, dest, a.offline)
        elif fix["kind"] == "project":
            fetch_project(fix, dest, a.offline)
        else:
            raise SystemExit(f"{fix['name']}: kind {fix['kind']!r} is neither `repo` nor `project`")
        prepare_note(fix, dest)


if __name__ == "__main__":
    main()
