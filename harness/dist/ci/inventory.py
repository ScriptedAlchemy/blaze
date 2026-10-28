#!/usr/bin/env python3
"""inventory.py: every Rust test tracedecay's Bazel runs, from its BUILD files at one commit.

    python3 -I harness/dist/ci/inventory.py --check d79de82273
                                      regenerate the inventory and fail if it differs from the
                                      checked-in one (docs/distributed.md, unit S2 [a-m2])
    python3 -I harness/dist/ci/inventory.py --rev d79de82273 --out FILE
                                      write the inventory to FILE
    python3 -I harness/dist/ci/inventory.py --rev d79de82273 --list
                                      print the counts and the tests kept on Bazel
    python3 -I harness/dist/ci/inventory.py --self-test
                                      the BUILD reader's rules on synthetic BUILD files

`harness/dist/ci/compare.py` reads the file this writes. It is the list GS compares a shadow run
against (docs/distributed.md section 10): every Rust test target, feature variant, custom harness,
doctest and generated fixture, with each test's timeout, retries and resource group as Bazel runs
it, and the tests that run rustc or cargo (the owner's `rustc-tests` gate, whose default keeps
them on Bazel).

**Where the bytes come from.** Every file is read from git at the commit, never from a working
tree: `git ls-tree` and one long-lived `git cat-file --batch`, so an owner's uncommitted edits
cannot change the inventory, and the inventory records the sha256 of every BUILD file it read. The
checkout is found by `--repo`, then `$TRACEDECAY_REPO`, `$TRACEDECAY`, `~/work/tracedecay` (where
`harness/dist/fetch.py` puts it) and `~/code/tracedecay` (the owner's checkout on the Mac). A
checkout without the commit fails saying so.

**What counts as a test.** A `rust_test(` rule in any `BUILD.bazel` is one test: rules_rust's
`rust_test` behind `//scripts:bazel/rust_test.bzl`'s isolating launcher. Its `tags = ["manual"]`
is recorded, because `bazel test //...` does not run a manual target. A target with
`use_libtest_harness = False` is a custom harness. A target with `crate =` is a unit test of that
crate (libtest over the crate's own `cfg(test)` modules); one without is an integration suite. A
doctest is a crate whose lib has doctests enabled and which Bazel does not run: there is no
`rust_doc_test` rule anywhere at this commit, so Bazel runs none while `cargo test` runs them all
(`doctest = false` turns them off, in 16 manifests). A generated fixture is a `genrule` output a
test consumes as `data`.

**The classes.** `blaze`: every test of the target runs on blaze's side of the shadow
comparison and on Bazel's, verdicts compared. `bazel`: the whole target stays on Bazel (the
`rustc-tests` gate's default), required executed there and absent from blaze. `doctest`: blaze
runs the crate's doctests and Bazel does not, so only blaze's verdict exists. `manual`:
`bazel test //...` does not run the target and neither side's comparison includes it. A target
whose tests are split, some run by blaze and some kept on Bazel, is class `blaze` and carries
`keeps_on_bazel`: the test names of the kept ones. That is the common case, because the gate is
about tests, not targets: `mcp_suite` has four tests that spawn rustc among hundreds that do not,
and keeping the whole target on Bazel would leave those hundreds uncompared.

**How a test's toolchain reach is found.** The files rustc compiles for the target are read from
its `mod` tree at the commit, not from its `srcs` glob (rules_rust hands the action every file;
rustc reads the tree). Each is scanned for a spawn of `rustc`, `cargo` or `rustdoc`
(`Command::new("rustc")` and the like) and for a read of `RUSTC`, `RUSTDOC`, `CARGO` or
`RUST_ANALYZER` (`var_os("RUSTC")` and the like), and each hit names the function it is in. Then
the `#[test]` functions that reach a hit are found by following calls inside each file: a test
that calls `compiler_diagnostic` reaches the `rustc` that function spawns. A hit in a shared
helper the target compiles in (`tests/common`) counts for the tests that call it, and is dropped
otherwise. The answer is `own` for a test that reaches such a hit, and the target records
`toolchain_reach: own` with each hit's file, line and function, and `keeps_on_bazel` with the test
names. A hit in product code the test may call is `product`: recorded and left in class `blaze`,
because whether the test reaches it is not decidable from the BUILD files, and a test that does
fails on blaze's side, where compare names it rather than hiding it. Nothing is guessed from a
name, and the closure is a superset: a call through a trait object or a macro that spells the name
differently is not followed, so a test it misses fails on blaze's side and is reported, never
quietly kept.

Python 3 standard library only. The inventory is one JSON object per line, sorted by label, so a
regeneration that changes one test changes one line.
"""
import argparse
import difflib
import hashlib
import json
import os
import re
import subprocess
import sys

FORMAT = 1
DEFAULT_GATE = "keep-on-bazel"
GATES = ("keep-on-bazel", "compat-lane", "change-tests")
SIZE_TIMEOUT = {"small": 60, "medium": 300, "large": 900, "enormous": 3600}
TIMEOUT_ATTR = {"short": 60, "moderate": 300, "long": 900, "eternal": 3600}
DEFAULT_SIZE = "medium"

# Bazel's per-test resources, as tracedecay's CI gives them (see `source.bazel` in the header).
RESOURCE_FLAGS = {
    "default_test_resources": "cpu=HOST_CPUS",
    "hosted_only_test_resources": "cpu=1",
    "exec_properties": {"cpu_count": "4"},
}

BUILD_NAMES = ("BUILD.bazel", "BUILD")
MAX_LABEL_DEPTH = 12


class Fatal(Exception):
    pass


# --------------------------------------------------------------------------------------------
# git: the only reader of the tree


class Repo(object):
    """A checkout read at one commit. Nothing is ever read from a working tree."""

    def __init__(self, path, commit):
        self.path = path
        self.commit = commit
        self._texts = {}
        self._batch = None
        self._tree = None
        if self.git("cat-file", "-e", commit + "^{commit}", check=False).returncode:
            raise Fatal("%s is not a commit in %s; fetch it there first" % (commit, path))
        self.commit = self.git("rev-parse", commit + "^{commit}").out.strip()

    def git(self, *args, **kw):
        check = kw.pop("check", True)
        ran = subprocess.run(["git", "-C", self.path] + list(args), capture_output=True,
                             stdin=subprocess.DEVNULL, **kw)
        if check and ran.returncode:
            raise Fatal("git %s failed in %s (%d): %s"
                        % (" ".join(args), self.path, ran.returncode,
                           ran.stderr.decode("utf-8", "replace").strip()))
        if ran.stdout is not None:
            ran.out = ran.stdout.decode("utf-8", "replace")
        return ran

    def tree(self):
        """Every file at the commit: path -> (mode, sha), in sorted path order."""
        if self._tree is None:
            raw = self.git("ls-tree", "-r", "-z", self.commit).stdout
            got = {}
            for entry in raw.split(b"\0"):
                if not entry:
                    continue
                head, path = entry.split(b"\t", 1)
                mode, kind, sha = head.decode().split()
                got[path.decode("utf-8")] = (mode, sha)
            self._tree = got
        return self._tree

    def files(self):
        return sorted(self.tree())

    def read(self, path):
        """A file's bytes at the commit, by path, from one long-lived `git cat-file --batch`."""
        sha = self.tree().get(path)
        if sha is None:
            raise Fatal("%s is not in %s at %s" % (path, self.path, self.commit[:12]))
        return self._blob(sha[1])

    def _blob(self, sha):
        if self._batch is None:
            self._batch = subprocess.Popen(["git", "-C", self.path, "cat-file", "--batch"],
                                           stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                           stderr=subprocess.DEVNULL)
        self._batch.stdin.write(sha.encode() + b"\n")
        self._batch.stdin.flush()
        head = self._batch.stdout.readline().decode().split()
        if len(head) != 3:
            raise Fatal("git cat-file --batch could not read %s: %r" % (sha, head))
        size = int(head[2])
        body = self._batch.stdout.read(size)
        self._batch.stdout.read(1)  # the newline after the body
        return body

    def text(self, path):
        if path not in self._texts:
            self._texts[path] = self.read(path).decode("utf-8", "replace")
        return self._texts[path]

    def build_files(self):
        return [p for p in self.files() if os.path.basename(p) in BUILD_NAMES]


def find_repo(explicit):
    """The tracedecay checkout, from --repo, the environment or the paths it is known at."""
    tried = []
    for cand in ([explicit] if explicit else []) + [
        os.environ.get("TRACEDECAY_REPO"),
        os.environ.get("TRACEDECAY"),
        os.path.join(os.path.expanduser("~"), "work", "tracedecay"),
        os.path.join(os.path.expanduser("~"), "code", "tracedecay"),
    ]:
        if not cand:
            continue
        cand = os.path.abspath(os.path.expanduser(cand))
        tried.append(cand)
        if os.path.isdir(os.path.join(cand, ".git")) or os.path.isfile(os.path.join(cand, ".git")):
            return cand
    raise Fatal("no tracedecay checkout: pass --repo DIR, or set TRACEDECAY_REPO, or run "
                "`python3 -I harness/dist/fetch.py tracedecay` (tried %s)" % ", ".join(tried))


# --------------------------------------------------------------------------------------------
# A reader for the Starlark subset BUILD files are written in


class Glob(object):
    def __init__(self, patterns, excludes):
        self.patterns, self.excludes = patterns, excludes


class Select(object):
    def __init__(self, branches):
        self.branches = branches


class Depset(object):
    def __init__(self, items):
        self.items = items


class Label(object):
    def __init__(self, text):
        self.text = text


class Unknown(object):
    """A call this reader does not evaluate (`all_crate_deps`, `aliases`, ...)."""

    def __init__(self, text):
        self.text = text


class Var(object):
    def __init__(self, name):
        self.name = name


TOKEN = re.compile(r"""
    (?P<space>[ \t\r\n]+)
  | (?P<comment>\#[^\n]*)
  | (?P<string>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')
  | (?P<int>\d+)
  | (?P<name>[A-Za-z_][A-Za-z0-9_]*)
  | (?P<punct>[\[\]{}(),:=+.%|&<>!*/@$~^-])
""", re.VERBOSE)

ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"', "'": "'", "0": "\0"}


def unquote(text):
    body, out, i = text[1:-1], [], 0
    while i < len(body):
        c = body[i]
        if c == "\\" and i + 1 < len(body):
            nxt = body[i + 1]
            if nxt in ESCAPES:
                out.append(ESCAPES[nxt])
                i += 2
                continue
            if nxt == "x" and i + 3 < len(body):
                try:
                    out.append(chr(int(body[i + 2:i + 4], 16)))
                    i += 4
                    continue
                except ValueError:
                    pass
            out.append(nxt)
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)


def tokenize(text):
    toks, at = [], 0
    while at < len(text):
        m = TOKEN.match(text, at)
        if not m:
            raise Fatal("BUILD text this reader cannot tokenize at %r"
                        % text[at:at + 40])
        at = m.end()
        kind = m.lastgroup
        if kind in ("space", "comment"):
            continue
        toks.append((kind, m.group()))
    return toks


class Parser(object):
    """Expressions and top-level statements, to the depth the BUILD files use."""

    def __init__(self, toks, env, where):
        self.toks, self.i, self.env, self.where = toks, 0, env, where

    def peek(self):
        return self.toks[self.i] if self.i < len(self.toks) else (None, None)

    def take(self, want=None):
        kind, text = self.peek()
        if kind is None:
            raise Fatal("%s: text ended inside an expression" % self.where)
        if want is not None and not (kind == "punct" and text == want):
            raise Fatal("%s: expected %r, found %r" % (self.where, want, text))
        self.i += 1
        return kind, text

    def expr(self):
        value = self.primary()
        while self.peek() == ("punct", "+"):
            self.take()
            value = concat(value, self.primary())
        return value

    def primary(self):
        kind, text = self.take()
        if kind == "string":
            value = unquote(text)
        elif kind == "int":
            value = int(text)
        elif kind == "name":
            if text in ("True", "False", "None"):
                value = {"True": True, "False": False, "None": None}[text]
            elif self.peek() == ("punct", "("):
                value = self.call(text)
            elif self.peek() == ("punct", "."):
                value = Unknown(text)  # an alias a `load` brought in: `vitest_bin.vitest_test(...)`
            elif text in self.env:
                value = self.env[text]
            elif text == "native":
                value = Var("native")
            else:
                value = Unknown(text)
        elif kind == "punct" and text == "[":
            value = []
            while self.peek() != ("punct", "]"):
                value.append(self.expr())
                if self.peek() == ("punct", ","):
                    self.take()
            self.take("]")
        elif kind == "punct" and text == "{":
            value = {}
            while self.peek() != ("punct", "}"):
                key = self.expr()
                self.take(":")
                value[key] = self.expr()
                if self.peek() == ("punct", ","):
                    self.take()
            self.take("}")
        else:
            raise Fatal("%s: %r starts no expression this reader knows" % (self.where, text))
        return self.postfix(value)

    def postfix(self, value):
        while self.peek() == ("punct", "."):
            self.take()
            _, name = self.take()
            args, kwargs = self.args()
            if isinstance(value, Var) and value.name == "native" and name == "package_relative_label":
                if args and isinstance(args[0], str):
                    return Label(args[0])
                raise Fatal("%s: native.package_relative_label needs a string" % self.where)
            if isinstance(value, Depset) and name == "to_list":
                return value.items
            value = Unknown("%s.%s(...)" % (getattr(value, "text", "?"), name))
        return value

    def call(self, name):
        args, kwargs = self.args()
        if name == "glob":
            patterns = args[0] if args else kwargs.get("patterns", [])
            excludes = kwargs.get("exclude", [])
            return Glob(as_list(patterns), as_list(excludes))
        if name == "select":
            branches = args[0] if args else {}
            if not isinstance(branches, dict):
                raise Fatal("%s: select() without a dict" % self.where)
            return Select([branches[k] for k in sorted(branches)])
        if name == "depset":
            items = []
            for arg in args:
                items.extend(as_list(arg))
            for key in sorted(kwargs):
                if key in ("items", "direct"):
                    items.extend(as_list(kwargs[key]))
            return Depset(items)
        shown = ", ".join([describe(a) for a in args] + ["%s=%s" % (k, describe(kwargs[k]))
                                                        for k in sorted(kwargs)])
        return Unknown("%s(%s)" % (name, shown))

    def args(self):
        self.take("(")
        args, kwargs = [], {}
        while self.peek() != ("punct", ")"):
            kind, text = self.peek()
            if kind == "name" and self.toks[self.i + 1:self.i + 2] == [("punct", "=")]:
                self.i += 2
                kwargs[text] = self.expr()
            else:
                args.append(self.expr())
            if self.peek() == ("punct", ","):
                self.take()
        self.take(")")
        return args, kwargs


def as_list(value):
    if value is None:
        return []
    if isinstance(value, list):
        return value
    if isinstance(value, Depset):
        return list(value.items)
    if isinstance(value, Select):
        out = []
        for branch in value.branches:
            out.extend(as_list(branch))
        return out
    return [value]


def concat(left, right):
    if isinstance(left, str) and isinstance(right, str):
        return left + right
    if isinstance(left, int) and isinstance(right, int) and not isinstance(left, bool):
        return left + right
    return as_list(left) + as_list(right)


def describe(value):
    if isinstance(value, str):
        return repr(value)
    if isinstance(value, (int, bool)) or value is None:
        return repr(value)
    if isinstance(value, list):
        return "[" + ", ".join(describe(v) for v in value) + "]"
    if isinstance(value, dict):
        return "{" + ", ".join("%s: %s" % (describe(k), describe(v))
                               for k, v in sorted(value.items(), key=lambda kv: str(kv[0]))) + "}"
    if isinstance(value, Glob):
        return "glob(%s, exclude=%s)" % (describe(value.patterns), describe(value.excludes))
    if isinstance(value, Label):
        return "label(%r)" % value.text
    return getattr(value, "text", repr(value))


class Rule(object):
    def __init__(self, kind, name, package, path, attrs, positional):
        self.kind, self.name, self.package = kind, name, package
        self.path, self.attrs, self.positional = path, attrs, positional

    @property
    def label(self):
        return "//%s:%s" % (self.package, self.name) if self.package else "//:%s" % self.name

    def attr(self, name, default=None):
        return self.attrs.get(name, default)

    def labels(self, name):
        out = []
        for v in as_list(self.attr(name)):
            if isinstance(v, Label):
                out.append(v.text)
            elif isinstance(v, str):
                out.append(v)
        return out


def parse_build(text, package, path):
    """Every rule of one BUILD file, in file order, with its variables resolved."""
    toks = tokenize(text)
    env, rules, at = {}, [], 0
    parser = Parser(toks, env, path)
    while parser.i < len(toks):
        kind, name = parser.take()
        if kind != "name":
            raise Fatal("%s: a statement starts with %r" % (path, name))
        if parser.peek() == ("punct", "="):
            parser.take()
            env[name] = parser.expr()
            continue
        if parser.peek() == ("punct", "."):
            # A rule from an alias a `load` brought in (`vitest_bin.vitest_test(...)`): read
            # and dropped, since only Rust tests are inventoried.
            parser.postfix(Unknown(name))
            continue
        if parser.peek() != ("punct", "("):
            raise Fatal("%s: %r is neither an assignment nor a call" % (path, name))
        args, kwargs = parser.args()
        if name == "load":
            continue
        rule_name = kwargs.get("name")
        if name == "exports_files":
            for arg in args:
                for item in as_list(arg):
                    if isinstance(item, str):
                        rules.append(Rule("exports_files", os.path.basename(item), package,
                                          path, {}, [item]))
            continue
        if not isinstance(rule_name, str):
            continue
        rules.append(Rule(name, rule_name, package, path, kwargs, args))
    return rules


# --------------------------------------------------------------------------------------------
# Resolving labels and globs against the tree at the commit


def glob_regex(pattern):
    out, i = [], 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:[^/]+/)*")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append("[^/]")
            i += 1
        elif pattern[i] == "[":
            end = pattern.find("]", i)
            if end < 0:
                out.append(re.escape(pattern[i]))
                i += 1
            else:
                body = pattern[i + 1:end]
                if body.startswith("!"):
                    body = "^" + body[1:]
                out.append("[" + body + "]")
                i = end + 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    return re.compile("^" + "".join(out) + "$")


class Builds(object):
    """Every rule of every BUILD file at the commit, indexed by label."""

    def __init__(self, repo):
        self.repo = repo
        self.files = repo.files()
        self.by_label, self.by_package, self.unresolved = {}, {}, set()
        self.package_files = {}
        for path in repo.files():
            package = os.path.dirname(path)
            parts = package.split("/") if package else []
            for depth in range(len(parts) + 1):
                self.package_files.setdefault("/".join(parts[:depth]), []).append(path)
        for path in repo.build_files():
            package = os.path.dirname(path)
            for rule in parse_build(repo.text(path), package, path):
                self.by_package.setdefault(package, []).append(rule)
                if rule.kind == "exports_files":
                    for item in rule.positional:
                        self.by_label.setdefault("//%s:%s" % (package, os.path.basename(item)),
                                                 rule)
                else:
                    self.by_label[rule.label] = rule

    def rule(self, label):
        return self.by_label.get(label)

    def split(self, label):
        if not label.startswith("//"):
            raise Fatal("not an absolute label: %r" % label)
        body = label[2:]
        if ":" in body:
            package, name = body.split(":", 1)
        else:
            package, name = body, os.path.basename(body)
        return package, name

    def resolve(self, package, value, seen=None):
        """The repository files a `srcs`/`data` value names, at the commit."""
        seen = set() if seen is None else seen
        if isinstance(value, Glob):
            got = []
            prefix = package + "/" if package else ""
            for pattern in value.patterns:
                rx = glob_regex(pattern)
                for path in self.package_files.get(package, []):
                    rel = path[len(prefix):] if path.startswith(prefix) else None
                    if rel is not None and rx.match(rel):
                        got.append(path)
            for pattern in value.excludes:
                rx = glob_regex(pattern)
                got = [p for p in got
                       if not rx.match(p[len(prefix):] if p.startswith(prefix) else p)]
            return sorted(set(got))
        if isinstance(value, Select):
            got = []
            for branch in value.branches:
                got.extend(self.resolve(package, branch, seen))
            return sorted(set(got))
        if isinstance(value, Depset):
            got = []
            for item in value.items:
                got.extend(self.resolve(package, item, seen))
            return sorted(set(got))
        if isinstance(value, list):
            got = []
            for item in value:
                got.extend(self.resolve(package, item, seen))
            return sorted(set(got))
        if isinstance(value, Label):
            value = value.text
        if isinstance(value, Unknown):
            self.unresolved.add(value.text)
            return []
        if not isinstance(value, str):
            self.unresolved.add(describe(value))
            return []
        if value.startswith("//"):
            return self.resolve_label(value, seen)
        if value.startswith(":"):
            return self.resolve_label("//%s%s" % (package, value), seen)
        if "*" in value or "?" in value or "[" in value:
            return self.resolve(package, Glob([value], []), seen)
        path = os.path.normpath(os.path.join(package, value)) if package else value
        return [path] if path in self.repo.tree() else []

    def resolve_label(self, label, seen):
        package, name = self.split(label)
        if label in seen or len(seen) > MAX_LABEL_DEPTH:
            return []
        seen = seen | {label}
        rule = self.rule(label)
        if rule is None:
            self.unresolved.add(label)
            return []
        if rule.kind == "exports_files":
            return [os.path.normpath(os.path.join(package, item))
                    for item in rule.positional]
        if rule.kind == "genrule":
            return []  # outputs are generated, not files at the commit
        for attr in ("srcs", "data"):
            if attr in rule.attrs:
                got = self.resolve(rule.package, rule.attr(attr), seen)
                if got:
                    return got
        return []

    def generated(self, package, value, seen=None):
        """The `genrule` targets a `data` value reaches, through filegroups."""
        seen = set() if seen is None else seen
        out = []
        if isinstance(value, list):
            for item in value:
                out.extend(self.generated(package, item, seen))
        elif isinstance(value, (Select, Depset)):
            for item in (value.branches if isinstance(value, Select) else value.items):
                out.extend(self.generated(package, item, seen))
        elif isinstance(value, Label):
            out.extend(self.generated(package, value.text, seen))
        elif isinstance(value, str) and (value.startswith("//") or value.startswith(":")):
            label = value if value.startswith("//") else "//%s%s" % (package, value)
            if label in seen:
                return []
            seen = seen | {label}
            rule = self.rule(label)
            if rule is None:
                return []
            if rule.kind == "genrule":
                return [label]
            for attr in ("srcs", "data"):
                if attr in rule.attrs:
                    out.extend(self.generated(rule.package, rule.attr(attr), seen))
        return sorted(set(out))


# --------------------------------------------------------------------------------------------
# The test inventory


SPAWN = re.compile(r'Command::new\(\s*(?:String::from\()?["\'](rustc|cargo|rustdoc)["\']')
ENV_READ = re.compile(r'\b(?:var_os|var|option_env)!?\(\s*"(RUSTC|RUSTDOC|CARGO|RUST_ANALYZER)"')

MOD_DECL = re.compile(r"(?m)^(?P<attrs>(?:[ \t]*#\[[^\n]*\]\s*\n)*)[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+"
                      r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)[ \t]*(?P<end>;|\{)")
PATH_ATTR = re.compile(r'#\[path[ \t]*=[ \t]*"([^"]+)"\]')
FN_DECL = re.compile(r"(?m)^[ \t]*(?:#\[[^\n]*\][ \t]*)*(?:pub(?:\([^)]*\))?[ \t]+)?"
                     r"(?:async[ \t]+)?(?:unsafe[ \t]+)?"
                     r"(?:extern[ \t]+\"[^\"]*\"[ \t]+)?fn[ \t]+([A-Za-z_][A-Za-z0-9_]*)")


def module_tree(repo, root, limit=4000, test_root=False):
    """Every file rustc compiles from `root`, with whether it is test code.

    A crate compiles the files its `mod` declarations name, not every file in `srcs`: rules_rust
    hands the action every file and rustc reads the module tree. So the reach of a test is read
    from the tree, following `mod name;` to `name.rs` or `name/mod.rs`, `#[path = "..."]`, and
    the `cfg(test)` flag an attribute above a declaration carries down. A module's children live
    beside `name.rs` in `name/`, and beside `name/mod.rs` in `name/`. `test_root` marks a tree
    that is test code whole, as an integration suite's is: it is only ever built as a test binary.
    """
    seen, out = set(), {}
    queue = [(root, test_root, os.path.dirname(root))]
    while queue and len(seen) < limit:
        path, test, children_dir = queue.pop(0)
        if path in seen or path not in repo.tree():
            continue
        seen.add(path)
        out[path] = test
        text = repo.text(path)
        for m in MOD_DECL.finditer(text):
            attrs, name, end = m.group("attrs") or "", m.group("name"), m.group("end")
            if end == "{":
                continue  # an inline module: its code is in this file
            child_test = test or "cfg(test)" in attrs.replace(" ", "")
            explicit = PATH_ATTR.search(attrs)
            if explicit:
                child = os.path.normpath(os.path.join(children_dir, explicit.group(1)))
                if child in repo.tree():
                    queue.append((child, child_test, os.path.dirname(child)))
                continue
            for cand in (os.path.join(children_dir, name + ".rs"),
                         os.path.join(children_dir, name, "mod.rs")):
                cand = os.path.normpath(cand)
                if cand in repo.tree():
                    sub = os.path.normpath(os.path.join(children_dir, name))
                    queue.append((cand, child_test, sub))
                    break
    return out


def enclosing_fn(text, line):
    """The name of the function a line is in, or None."""
    name = None
    for number, body in enumerate(text.split("\n"), 1):
        m = FN_DECL.match(body)
        if m:
            name = m.group(1)
        if number == line:
            return name
    return name


def scan_sources(repo, paths):
    """Every place a compiled source spawns or names a compiler, with its line."""
    hits = []
    for path, is_test in sorted(paths.items()):
        text = repo.text(path)
        for number, line in enumerate(text.split("\n"), 1):
            found = []
            for m in SPAWN.finditer(line):
                found.append({"file": path, "line": number, "what": m.group(1),
                              "how": "spawn", "test_code": is_test})
            for m in ENV_READ.finditer(line):
                found.append({"file": path, "line": number, "what": m.group(1),
                              "how": "env", "test_code": is_test})
            for hit in found:
                hit["function"] = enclosing_fn(text, number)
                hits.append(hit)
    return hits


def crate_sources(builds, rule):
    """The crate root a test compiles, and the library target it tests, if it names one."""
    crate = rule.attr("crate")
    if isinstance(crate, str):
        label = crate if crate.startswith("//") else "//%s%s" % (rule.package, crate)
        target = builds.rule(label)
        if target is not None and "srcs" in target.attrs:
            return target, crate_root_of(builds, rule, target)
    return None, crate_root_of(builds, rule, None)


def crate_root_of(builds, rule, target):
    for holder in (rule, target):
        if holder is None:
            continue
        root = holder.attr("crate_root")
        if isinstance(root, str):
            if root.startswith("//"):
                package, name = builds.split(root)
                return os.path.normpath(os.path.join(package, name))
            return os.path.normpath(os.path.join(holder.package, root))
    if target is not None and target.kind == "rust_library":
        return os.path.normpath(os.path.join(target.package, "src", "lib.rs"))
    return None


def is_test_code(path):
    """Whether a file's own path marks it as test code, for the shared-helper rule."""
    return bool(re.search(r"(?:^|/)(?:tests?|benches)/", path))


CFG_TEST_MOD = re.compile(r"(?m)^[ \t]*#\[cfg\(test\)\][ \t]*\n[ \t]*(?:pub[ \t]+)?mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*\{")


def cfg_test_regions(text):
    """The text inside every `#[cfg(test)] mod ... { ... }` block of one file."""
    out = []
    for m in CFG_TEST_MOD.finditer(text):
        depth, at = 0, m.end() - 1
        while at < len(text):
            if text[at] == "{":
                depth += 1
            elif text[at] == "}":
                depth -= 1
                if depth == 0:
                    out.append(text[m.end():at])
                    break
            at += 1
    return out


def reach_of(hits, own_dir, own_texts, test_regions):
    """`own` for a hit the test runs itself, `product` for one in product code it may call.

    Test code is a file under a `tests/` directory (an integration suite), a module behind
    `#[cfg(test)]`, or a shared helper those call. A hit in test code is the test's own when the
    file is in the target's own directory, or when the function it is in is called from the
    target's own files or from a `#[cfg(test)]` block: every suite compiles `tests/common`, but
    only the ones that call it need a toolchain. A hit in test code nothing calls is dropped. A
    hit in product code is `product`: recorded, and left in class `blaze`, because whether the
    test reaches it is not decidable from the BUILD files, and a test that does reach it fails on
    blaze's side, where compare names it rather than hiding it.
    """
    kept, kinds = [], set()
    callers = list(own_texts) + list(test_regions)
    for hit in hits:
        path = hit["file"]
        if hit.get("test_code") or is_test_code(path):
            if own_dir and (path == own_dir or path.startswith(own_dir + "/")):
                kept.append(hit)
                kinds.add("own")
                continue
            fn = hit.get("function")
            if fn and any(called_from(fn, text) for text in callers):
                kept.append(hit)
                kinds.add("own")
                continue
            continue
        fn = hit.get("function")
        if fn and len(fn) > 4 and any(called_from(fn, text) for text in test_regions):
            kept.append(hit)
            kinds.add("own")
            continue
        kept.append(hit)
        kinds.add("product")
    for kind in ("own", "product"):
        if kind in kinds:
            return kind, kept
    return None, []


def called_from(name, text):
    return re.search(r"\b%s\s*\(" % re.escape(name), text) is not None


TEST_ATTR = re.compile(r"#\[(?:tokio::)?test\b|#\[test_case|#\[rstest\]|#\[should_panic")


def functions_with_calls(text):
    """Every function in one file, with whether it is a test and the functions it calls."""
    lines = text.split("\n")
    found, starts = [], []
    for index, line in enumerate(lines):
        m = FN_DECL.match(line)
        if not m:
            continue
        above = [l for l in lines[max(0, index - 6):index] if l.strip()]
        is_test = bool(TEST_ATTR.search(line) or (above and TEST_ATTR.search(above[-1])))
        starts.append((index, m.group(1), is_test))
    for at, (index, name, is_test) in enumerate(starts):
        end = starts[at + 1][0] if at + 1 < len(starts) else len(lines)
        found.append((name, is_test, "\n".join(lines[index:end])))
    return found


def test_functions_reaching(files, hits, depth=8):
    """The `#[test]` functions of each file that reach a hit's function, by name.

    The closure stays inside one file, because a name alone is not a function: these trees hold
    many `run`, `setup` and `fixture` helpers, and a graph over names alone connects everything to
    everything. A test reaches a hit when its body, or the body of a function in the same file it
    calls (transitively), names the function the hit is in. A hit in a shared helper is a separate
    question, answered by `reach_of`. This is a superset: a call through a trait object or a
    macro that spells the name differently is not followed, so the gate stays on the safe side.
    """
    targets = {hit.get("function") for hit in hits if hit.get("function")}
    if not targets:
        return []
    out = []
    for path in sorted(files):
        bodies, tests = {}, {}
        for name, is_test, body in functions_with_calls(files[path]):
            bodies.setdefault(name, []).append(body)
            if is_test:
                tests.setdefault(name, []).append(path)
        reached = set(targets)
        for _ in range(depth):
            grew = False
            for name, chunks in bodies.items():
                if name in reached:
                    continue
                if any(called_from(other, chunk) for other in reached for chunk in chunks):
                    reached.add(name)
                    grew = True
            if not grew:
                break
        out.extend(name for name in tests if name in reached)
    return sorted(set(out))


def doctest_crates(repo, builds):
    """Crates whose lib has doctests and which Bazel never runs a doctest of.

    The lib's existence comes from the crate's BUILD file (a `rust_library`), and `doctest =
    false` in the manifest turns them off. Bazel runs no doctest at all at this commit: there is
    no `rust_doc_test` rule anywhere, while `cargo test` and `cargo blaze test` run each lib's.
    """
    out = []
    for package in sorted({rule.package for rule in builds.by_label.values()
                           if rule.kind == "rust_library"}):
        manifest = os.path.join(package, "Cargo.toml")
        if manifest not in repo.tree():
            continue
        body = repo.text(manifest)
        lib = re.search(r"(?m)^\[lib\]\s*$(.*?)(?=^\[|\Z)", body, re.S)
        if lib and re.search(r"(?m)^\s*doctest\s*=\s*false\s*$", lib.group(1)):
            continue
        out.append({"label": "//%s:doctest" % package, "package": package})
    return out


def build_inventory(repo, gate):
    """Every record of the inventory, in label order."""
    builds = Builds(repo)
    rules = [r for r in builds.by_label.values() if r.kind == "rust_test"]
    rules.sort(key=lambda r: r.label)
    tests = []
    for rule in rules:
        target, root = crate_sources(builds, rule)
        unit = isinstance(rule.attr("crate"), str)
        modules = module_tree(repo, root, test_root=not unit) if root else {}
        hits = scan_sources(repo, modules)
        own_dir = os.path.dirname(root) if root else None
        own_texts = [repo.text(p) for p in modules
                     if own_dir and (p == own_dir or p.startswith(own_dir + "/"))]
        test_regions = []
        for path in sorted(modules):
            test_regions.extend(cfg_test_regions(repo.text(path)))
        reach, hits = reach_of(hits, own_dir, own_texts, test_regions)
        files = {path: repo.text(path) for path in modules}
        reaching = test_functions_reaching(files, hits)
        all_tests = sum(1 for path in files
                        for _, is_test, _ in functions_with_calls(files[path]) if is_test)
        other_tests = all_tests > len(reaching)
        features = [v for v in as_list(rule.attr("crate_features")) if isinstance(v, str)]
        tags = [v for v in as_list(rule.attr("tags")) if isinstance(v, str)]
        size = rule.attr("size") if isinstance(rule.attr("size"), str) else DEFAULT_SIZE
        timeout = rule.attr("timeout")
        if isinstance(timeout, str) and timeout in TIMEOUT_ATTR:
            timeout_s, timeout_source = TIMEOUT_ATTR[timeout], "attr timeout " + timeout
        else:
            timeout_s = SIZE_TIMEOUT.get(size, SIZE_TIMEOUT[DEFAULT_SIZE])
            timeout_source = "bazel size " + size
        flaky = rule.attr("flaky") is True
        # `rust_test` in rust_test.bzl starts every test at cpu_count 4 and lets the target
        # override it, which is what the executor is told this test needs.
        exec_props = dict(RESOURCE_FLAGS["exec_properties"])
        for key, value in (rule.attr("exec_properties") or {}).items():
            if isinstance(value, str):
                exec_props[key] = value
        runfiles_env = {}
        for key, value in sorted((rule.attr("runfiles_env") or {}).items()):
            runfiles_env[key] = value.text if isinstance(value, Label) else str(value)
        generated = builds.generated(rule.package, rule.attr("data", []))
        manual = "manual" in tags
        harness = "custom" if rule.attr("use_libtest_harness") is False else "libtest"
        if isinstance(rule.attr("crate"), str):
            kind = "unit"
        elif root and "/benches/" in "/" + root:
            kind = "bench"
        else:
            kind = "integration"
        # Which of this target's tests the `rustc-tests` gate keeps on Bazel. Bazel runs the
        # target whole, so its side runs every test; blaze runs the target with these filtered
        # out, and they are the ones GS requires executed on Bazel and absent from blaze.
        keeps = list(reaching) if reach == "own" else []
        gate_name = "rustc-tests" if keeps else None
        if manual:
            klass, why = "manual", "tags = [\"manual\"]: `bazel test //...` does not run it"
            keeps = []
        elif keeps and gate != "keep-on-bazel":
            klass, why = "blaze", ("needs rustc/cargo (%s); the rustc-tests gate is set to %s, so "
                                   "blaze runs them" % (reach, gate))
            keeps = []
        elif keeps and len(keeps) == len(reaching) and not other_tests:
            klass, why = "bazel", ("every test in it needs rustc/cargo (%s); the rustc-tests "
                                   "gate's default keeps it on Bazel" % reach)
        elif keeps:
            klass, why = "blaze", ("partly kept on Bazel: %d test%s run or need rustc/cargo (%s), "
                                   "the rustc-tests gate's default" % (len(keeps),
                                                                      "" if len(keeps) == 1
                                                                      else "s", reach))
        else:
            klass, why = "blaze", None
        tests.append({
            "record": "test",
            "label": rule.label,
            "package": rule.package,
            "name": rule.name,
            "kind": kind,
            "harness": harness,
            "class": klass,
            "why": why,
            "gate": gate_name,
            "keeps_on_bazel": keeps,
            "features": features,
            "crate": rule.attr("crate") if isinstance(rule.attr("crate"), str) else None,
            "crate_root": root,
            "sources": len(modules),
            "tags": tags,
            "size": size,
            "timeout_s": timeout_s,
            "timeout_source": timeout_source,
            "attempts": 3 if flaky else 1,
            "retries": 2 if flaky else 0,
            "resource_group": exec_props,
            "default_test_resources": RESOURCE_FLAGS["default_test_resources"],
            "shard_count": rule.attr("shard_count") if isinstance(rule.attr("shard_count"), int) else 1,
            "local": rule.attr("local") is True,
            "runfiles_env": runfiles_env,
            "generated_data": generated,
            "toolchain_reach": reach,
            "toolchain_tests": keeps,
            "toolchain_runs": hits,
        })
    for crate in doctest_crates(repo, builds):
        tests.append({
            "record": "test",
            "label": crate["label"],
            "package": crate["package"],
            "name": "doctest",
            "kind": "doctest",
            "harness": "rustdoc",
            "class": "doctest",
            "why": "Bazel has no rust_doc_test rule, so it runs no doctest; `cargo test` and "
                   "`cargo blaze test` do",
            "gate": None,
            "keeps_on_bazel": [],
            "features": [],
            "crate": None,
            "crate_root": None,
            "sources": 0,
            "tags": [],
            "size": None,
            "timeout_s": None,
            "timeout_source": None,
            "attempts": 1,
            "retries": 0,
            "resource_group": None,
            "default_test_resources": None,
            "shard_count": 1,
            "local": False,
            "runfiles_env": {},
            "generated_data": [],
            "toolchain_reach": None,
            "toolchain_tests": [],
            "toolchain_runs": [],
        })
    tests.sort(key=lambda t: t["label"])
    fixtures = {}
    for test in tests:
        for label in test["generated_data"]:
            fixtures.setdefault(label, set()).add(test["label"])
    records = [header(repo, gate, builds, tests, fixtures)]
    records.extend(tests)
    for label in sorted(fixtures):
        rule = builds.rule(label)
        records.append({
            "record": "fixture",
            "label": label,
            "package": builds.split(label)[0],
            "rule": rule.kind if rule else None,
            "consumers": sorted(fixtures[label]),
            "why": "generated by a genrule and read by these tests as data, so a worker needs "
                   "its output as part of the test's bundle",
        })
    return records


def counts_of(tests, fixtures):
    run = [t for t in tests if t["class"] != "manual"]
    variants = set()
    for test in run:
        variants.add((test["package"], tuple(test["features"])))
    sizes = {}
    for test in run:
        if test["size"]:
            sizes[test["size"]] = sizes.get(test["size"], 0) + 1
    return {
        "test_records": len(tests),
        "fixture_records": len(fixtures),
        "rust_test_targets": sum(1 for t in tests if t["kind"] != "doctest"),
        "run_by_bazel": sum(1 for t in tests if t["class"] != "manual" and t["kind"] != "doctest"),
        "manual": sum(1 for t in tests if t["class"] == "manual"),
        "blaze": sum(1 for t in tests if t["class"] == "blaze"),
        "bazel": sum(1 for t in tests if t["class"] == "bazel"),
        "kept_on_bazel_tests": sum(len(t.get("keeps_on_bazel") or []) for t in tests),
        "targets_partly_kept": sum(1 for t in tests if t["class"] != "bazel"
                                   and t.get("keeps_on_bazel")),
        "doctest": sum(1 for t in tests if t["class"] == "doctest"),
        "custom_harness": sum(1 for t in run if t["harness"] == "custom"),
        "custom_harness_manual": sum(1 for t in tests if t["class"] == "manual"
                                     and t["harness"] == "custom"),
        "libtest": sum(1 for t in run if t["harness"] == "libtest"),
        "unit": sum(1 for t in run if t["kind"] == "unit"),
        "integration": sum(1 for t in run if t["kind"] == "integration"),
        "bench": sum(1 for t in tests if t["kind"] == "bench"),
        "feature_variants": len(variants),
        "generated_fixtures": len(fixtures),
        "sizes": sizes,
        "product_toolchain_reach": sum(1 for t in run if t["toolchain_reach"] == "product"),
    }


def header(repo, gate, builds, tests, fixtures):
    return {
        "record": "inventory",
        "format": FORMAT,
        "generated_by": "harness/dist/ci/inventory.py",
        "about": "every Rust test tracedecay's Bazel runs, from its BUILD files at one commit "
                 "(docs/distributed.md, unit S2). `compare.py` reads it. The owner approves it "
                 "by setting `approved` to true.",
        "approved": False,
        "gate": gate,
        "gates": list(GATES),
        "source": {
            "repo": "ScriptedAlchemy/tracedecay",
            "commit": repo.commit,
            "tree": repo.git("rev-parse", repo.commit + "^{tree}").out.strip(),
            "build_files": [{"path": p, "sha256": hashlib.sha256(repo.read(p)).hexdigest()}
                            for p in repo.build_files()],
            "bazel": {
                "test_pattern": "//...",
                "workflow": ".github/workflows/ci.yml",
                "default_test_resources": RESOURCE_FLAGS["default_test_resources"],
                "hosted_only_test_resources": RESOURCE_FLAGS["hosted_only_test_resources"],
                "exec_properties": RESOURCE_FLAGS["exec_properties"],
                "nextest": ".config/nextest.toml [profile.ci]: retries = 1, flaky-result = "
                           "\"fail\"; nextest is not what Bazel runs, and blaze keeps it",
            },
        },
        "counts": counts_of(tests, fixtures),
    }


def render(records):
    return "".join(json.dumps(record, sort_keys=True) + "\n" for record in records)


def inventory_path(rev, here):
    return os.path.join(here, "inventory.%s.json" % rev[:10])


# --------------------------------------------------------------------------------------------
# Self-test: the reader's rules, on BUILD text written here


SELFTEST_BUILD = '''
load("@rules_rust//rust:defs.bzl", "rust_binary")
load("//scripts:bazel/rust_test.bzl", "rust_test")

SDK_SOURCES = ["a.rs", "b.rs"]

rust_library(
    name = "lib__full",
    srcs = glob(["src/**/*.rs"], exclude = ["src/broken/**"], allow_empty = True)
        + ["//other:generated_sources"],
    crate_features = ["full", "lite"],
    edition = "2024",
)

rust_test(
    name = "unit_test",
    crate = ":lib__full",
    crate_features = ["full", "lite"],
    size = "large",
    exec_properties = {"cpu_count": "2", "Pool": "mac"},
    runfiles_env = {"CARGO_BIN_EXE_x": "//other:bin"},
)

rust_test(
    name = "suite",
    srcs = glob(["tests/suite/**/*.rs"]) + glob(["tests/common/**/*.rs"]),
    crate_root = "tests/suite/main.rs",
    timeout = "eternal",
    tags = ["manual"],
    use_libtest_harness = False,
)

rust_test(
    name = "needs_toolchain",
    srcs = glob(["tests/suite/**/*.rs"]) + glob(["tests/common/**/*.rs"]),
    crate_root = "tests/suite/main.rs",
)

rust_test(
    name = "picked",
    srcs = select({
        "//:a": ["one.rs"],
        "//:b": ["two.rs"],
    }),
)

filegroup(
    name = "generated_sources",
    srcs = depset(["gen/a.rs"]).to_list(),
)

genrule(
    name = "stamp",
    srcs = [],
    outs = ["stamp.txt"],
    cmd = "date > $@",
)
'''


def selftest():
    """The reader and the class rules, on a BUILD file whose answers are written here."""
    tree = {
        "crates/x/BUILD.bazel": SELFTEST_BUILD,
        "crates/x/src/lib.rs": "pub fn f() {}\n",
        "crates/x/src/broken/bad.rs": "this is excluded\n",
        "crates/x/tests/suite/main.rs": '#[path = "../common/mod.rs"]\nmod common;\nmod one;\nfn main() {}\n',
        "crates/x/tests/suite/one.rs": "#[test] fn t() { common::env(); common::run(); }\n",
        "crates/x/tests/suite/common.rs": "pub fn env() {}\npub fn run() {}\n",
        "crates/x/tests/common/mod.rs": (
            "fn env() { let _ = std::env::var_os(\"RUSTC\"); }\n"
            "fn run() { std::process::Command::new(\"rustc\"); }\n"),
        "crates/x/one.rs": "fn main() {}\n",
        "crates/x/two.rs": "fn main() {}\n",
        "other/BUILD.bazel": (
            'filegroup(name = "generated_sources", srcs = glob(["gen/**/*.rs"]))\n'
            'rust_binary(name = "bin", srcs = ["bin.rs"])\n'),
        "other/gen/a.rs": "pub fn g() {}\n",
        "other/bin.rs": "fn main() {}\n",
        "Cargo.toml": '[workspace]\nmembers = ["crates/x"]\n',
        "crates/x/Cargo.toml": '[package]\nname = "x"\n[lib]\npath = "src/lib.rs"\n',
    }

    class FakeRepo(object):
        commit = "0" * 40

        def files(self):
            return sorted(tree)

        def tree(self):
            return {p: ("100644", p) for p in tree}

        def read(self, path):
            return tree[path].encode()

        def text(self, path):
            return tree[path]

        def build_files(self):
            return [p for p in sorted(tree) if os.path.basename(p) == "BUILD.bazel"]

        def git(self, *args, **kw):
            class Ran(object):
                returncode = 0
                stdout = ("1" * 40 + "\n").encode()
                out = "1" * 40 + "\n"
                stderr = b""
            return Ran()

    failures = []
    checks = []

    def check(name, got, want):
        checks.append(name)
        if got != want:
            failures.append("%s: got %r, want %r" % (name, got, want))

    builds = Builds(FakeRepo())
    labels = sorted(builds.by_label)
    check("labels", labels, ["//crates/x:generated_sources", "//crates/x:lib__full",
                             "//crates/x:needs_toolchain", "//crates/x:picked",
                             "//crates/x:stamp", "//crates/x:suite", "//crates/x:unit_test",
                             "//other:bin", "//other:generated_sources"])
    check("glob", builds.resolve("crates/x", builds.rule("//crates/x:lib__full").attr("srcs")),
          ["crates/x/src/lib.rs", "other/gen/a.rs"])
    check("glob excludes", "crates/x/src/broken/bad.rs" in
          builds.resolve("crates/x", Glob(["src/**/*.rs"], ["src/broken/**"])), False)
    check("select", builds.resolve("crates/x", builds.rule("//crates/x:picked").attr("srcs")),
          ["crates/x/one.rs", "crates/x/two.rs"])
    check("depset", builds.resolve("other", builds.rule("//other:generated_sources").attr("srcs")),
          ["other/gen/a.rs"])
    unit = builds.rule("//crates/x:unit_test")
    check("unit target", crate_sources(builds, unit)[0].label, "//crates/x:lib__full")
    check("unit root", crate_sources(builds, unit)[1], "crates/x/src/lib.rs")
    check("unit exec", unit.attr("exec_properties"), {"cpu_count": "2", "Pool": "mac"})
    check("unit size", unit.attr("size"), "large")
    check("unit runfiles", unit.attr("runfiles_env")["CARGO_BIN_EXE_x"], "//other:bin")
    suite = builds.rule("//crates/x:suite")
    check("suite timeout", suite.attr("timeout"), "eternal")
    check("suite harness", suite.attr("use_libtest_harness"), False)
    check("suite tags", suite.attr("tags"), ["manual"])
    check("suite generated", builds.generated("crates/x", suite.attr("data", [])), [])
    check("genrule outs", builds.resolve("crates/x", ":stamp"), [])
    check("glob zero dirs", glob_regex("src/**/*.rs").match("src/a.rs") is not None, True)
    check("glob star", glob_regex("src/*.rs").match("src/a/b.rs"), None)
    check("glob class", glob_regex("[ab].rs").match("b.rs") is not None, True)
    hits = scan_sources(FakeRepo(), {"crates/x/tests/common/mod.rs": False})
    check("scan", [(h["what"], h["how"], h["line"]) for h in hits],
          [("RUSTC", "env", 1), ("rustc", "spawn", 2)])
    check("reach own", reach_of(hits, "crates/x/tests/common", [], [])[0], "own")
    check("reach product", reach_of([{"file": "crates/x/src/run.rs", "test_code": False}],
                                    "crates/x/tests/suite", [], [])[0], "product")
    check("reach dropped", reach_of([{"file": "crates/x/tests/common/mod.rs",
                                      "test_code": True, "function": "unused"}],
                                    "crates/x/tests/suite", ["fn other() {}"], [])[1], [])
    check("reach called", reach_of([{"file": "crates/x/tests/common/mod.rs",
                                     "test_code": True, "function": "helper"}],
                                   "crates/x/tests/suite", ["helper();"], [])[0], "own")
    check("reach cfg(test)", reach_of([{"file": "crates/x/src/run.rs", "test_code": False,
                                        "function": "spawns"}], None, [],
                                      ["spawns();"])[0], "own")
    check("cfg regions", cfg_test_regions(
        "#[cfg(test)]\nmod tests {\nfn a() { b(); }\n}\nfn b() {}\n"),
          ["\nfn a() { b(); }\n"])
    check("test code", is_test_code("crates/x/src/daemon/tests/feedback.rs"), True)
    check("product code", is_test_code("crates/x/src/daemon/service.rs"), False)
    check("module tree", module_tree(FakeRepo(), "crates/x/tests/suite/main.rs"),
          {"crates/x/tests/suite/main.rs": False, "crates/x/tests/common/mod.rs": False,
           "crates/x/tests/suite/one.rs": False})
    records = build_inventory(FakeRepo(), DEFAULT_GATE)
    check("header", records[0]["record"], "inventory")
    classes = {r["label"]: r["class"] for r in records if r["record"] == "test"}
    check("classes", classes, {"//crates/x:doctest": "doctest",
                               "//crates/x:needs_toolchain": "bazel",
                               "//crates/x:picked": "blaze",
                               "//crates/x:suite": "manual", "//crates/x:unit_test": "blaze"})
    unit = [r for r in records if r.get("label") == "//crates/x:unit_test"][0]
    check("unit timeout", (unit["timeout_s"], unit["timeout_source"]), (900, "bazel size large"))
    check("unit attempts", (unit["attempts"], unit["retries"]), (1, 0))
    check("unit resources", unit["default_test_resources"], "cpu=HOST_CPUS")
    check("unit class", unit["class"], "blaze")
    needs = [r for r in records if r.get("label") == "//crates/x:needs_toolchain"][0]
    check("needs toolchain", (needs["toolchain_reach"], needs["class"], needs["gate"]),
          ("own", "bazel", "rustc-tests"))
    suite = [r for r in records if r.get("label") == "//crates/x:suite"][0]
    check("suite timeout", (suite["timeout_s"], suite["timeout_source"]), (3600, "attr timeout eternal"))
    check("suite harness", suite["harness"], "custom")
    doctest = [r for r in records if r.get("label") == "//crates/x:doctest"]
    check("doctest why", bool(doctest) and "rust_doc_test" in doctest[0]["why"], True)
    for failure in failures:
        print("inventory.py --self-test: " + failure)
    print("inventory.py --self-test: %d checks, %d failed" % (len(checks), len(failures)))
    return 1 if failures else 0


# --------------------------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", help="the tracedecay checkout (default: $TRACEDECAY_REPO, "
                                   "$TRACEDECAY, ~/work/tracedecay, ~/code/tracedecay)")
    ap.add_argument("--rev", default="d79de82273", help="the commit to inventory (default %(default)s)")
    ap.add_argument("--gate", default=DEFAULT_GATE, choices=GATES,
                    help="the owner's rustc-tests gate (default %(default)s)")
    ap.add_argument("--out", help="write the inventory here (default: the checked-in file)")
    ap.add_argument("--check", metavar="COMMIT",
                    help="regenerate COMMIT's inventory and fail if it differs from the "
                         "checked-in one")
    ap.add_argument("--list", action="store_true", help="print the counts, and the tests kept on Bazel")
    ap.add_argument("--self-test", action="store_true", help="the BUILD reader's rules")
    a = ap.parse_args()
    here = os.path.dirname(os.path.abspath(__file__))
    if a.self_test:
        return selftest()
    rev = a.check or a.rev
    repo = Repo(find_repo(a.repo), rev)
    records = build_inventory(repo, a.gate)
    text = render(records)
    if a.check:
        path = inventory_path(repo.commit, here)
        if not os.path.exists(path):
            print("inventory.py: %s is not checked in; write it with --rev %s --out %s"
                  % (os.path.basename(path), rev, path), file=sys.stderr)
            return 1
        with open(path) as f:
            checked = f.read()
        if checked != text:
            diff = list(difflib.unified_diff(checked.split("\n"), text.split("\n"),
                                             "checked-in " + os.path.basename(path),
                                             "regenerated at " + repo.commit[:12], n=1))
            print("inventory.py: the inventory differs from the checked-in one (%d diff lines)"
                  % len(diff), file=sys.stderr)
            for line in diff[:80]:
                print(line, file=sys.stderr)
            return 1
        print("inventory.py: %s matches %s at %s (%d records)"
              % (os.path.basename(path), "d79de82273", repo.commit[:12], len(records)))
        return 0
    path = a.out or inventory_path(repo.commit, here)
    with open(path, "w") as f:
        f.write(text)
    counts = records[0]["counts"]
    print("inventory.py: %d records to %s" % (len(records), path))
    for key in sorted(counts):
        value = counts[key]
        if isinstance(value, dict):
            print("  %-26s %s" % (key, ", ".join("%s %d" % (k, value[k]) for k in sorted(value))))
        else:
            print("  %-26s %d" % (key, value))
    if a.list:
        for record in records:
            if record["record"] != "test":
                continue
            if record["class"] == "bazel":
                print("bazel      %s (the whole target)" % record["label"])
            for name in record.get("keeps_on_bazel") or []:
                print("bazel      %s: %s" % (record["label"], name))
            if record["class"] == "manual":
                print("manual     %s" % record["label"])
            for hit in record["toolchain_runs"]:
                print("           %s:%d %s %s in %s()"
                      % (hit["file"], hit["line"], hit["how"], hit["what"],
                         hit.get("function") or "?"))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Fatal as error:
        print("inventory.py: " + str(error), file=sys.stderr)
        sys.exit(2)
