#!/usr/bin/env python3
"""Exercise release smoke selection and provenance with process doubles, never a compiler."""
import contextlib
from dataclasses import replace
import importlib.util
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "promote.yaml"
SCRIPT = WORKFLOW.parents[2] / "harness" / "smoke.py"
SPEC = importlib.util.spec_from_file_location("release_smoke_corpus", SCRIPT)
assert SPEC and SPEC.loader
smoke = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = smoke
SPEC.loader.exec_module(smoke)


def smoke_step(name="what it installed runs"):
    lines = WORKFLOW.read_text().splitlines()
    heading = f"      - name: {name}"
    if lines.count(heading) != 1:
        raise AssertionError(f"promote needs exactly one {name} smoke step")
    at = lines.index(heading) + 1
    while at < len(lines) and lines[at] != "        run: |":
        if lines[at].startswith("      - "):
            raise AssertionError(f"{name} smoke must have a shell script")
        at += 1
    if at == len(lines):
        raise AssertionError(f"{name} smoke must have a shell script")
    body = []
    for line in lines[at + 1:]:
        if line and not line.startswith("          "):
            break
        body.append(line[10:] if line else "")
    return "\n".join(body) + "\n"


STUB = r"""#!/bin/sh
printf '%s\n' "$1" >> "$CARGO_HOME/commands"
case "$1" in
  --version)
    printf 'blaze 0.20261105.4 (test build)\n'
    ;;
  run)
    test -f Cargo.toml && test -f src/main.rs &&
    grep -Fq 'name = "hello"' Cargo.toml &&
    grep -Fq 'edition = "2021"' Cargo.toml &&
    grep -Fq 'println!("Hello, world!")' src/main.rs || exit 11
    printf 'Hello, world!\n'
    ;;
  stop) ;;
  *) printf 'unsupported blaze command: %s\n' "$1" >&2; exit 12 ;;
esac
"""


class ReleaseSmokeTests(unittest.TestCase):
    def exercise(self, transform=lambda body: body):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            blaze = bin_dir / "blaze"
            blaze.write_text(STUB)
            blaze.chmod(0o700)
            env = dict(os.environ, CARGO_HOME=str(root), RUNNER_TEMP=str(root),
                       TAG="v0.20261105.4", PATH=str(bin_dir) + os.pathsep + os.environ["PATH"])
            done = subprocess.run(["bash", "-eo", "pipefail", "-c", transform(smoke_step())],
                                  env=env, cwd=root, text=True, capture_output=True, timeout=10)
            commands = (root / "commands")
            return done, commands.read_text().splitlines() if commands.exists() else []

    def test_build_and_run_from_sources_without_removed_new_command(self):
        done, commands = self.exercise()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(commands, ["--version", "run", "stop"])
        self.assertIn("Hello, world!", done.stdout)

    def test_plant_the_removed_new_command_fails(self):
        done, commands = self.exercise(
            lambda body: body.replace('mkdir -p "$RUNNER_TEMP/hello/src"',
                                      'blaze new --quiet hello'))
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("unsupported blaze command: new", done.stderr)
        self.assertNotIn("run", commands)

    def test_plant_missing_manifest_fails(self):
        done, commands = self.exercise(
            lambda body: body.replace("> Cargo.toml", "> wrong-manifest.toml"))
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("run", commands)
        self.assertNotIn("stop", commands)


    def test_linux_smoke_raises_memlock_before_starting_released_daemon(self):
        smoke = WORKFLOW.read_text().split("\n  smoke:\n", 1)[1].split("\n  apps:\n", 1)[0]
        for name in ("locked memory for released daemon", "locked memory inherited by smoke"):
            self.assertEqual(smoke.count(f"      - name: {name}"), 1)
        self.assertLess(smoke.index("      - name: locked memory for released daemon"),
                        smoke.index("      - name: locked memory inherited by smoke"))
        self.assertLess(smoke.index("      - name: locked memory inherited by smoke"),
                        smoke.index("      - name: what it installed runs"))

        # Exercise the actual workflow shell with fake process tools. Losing the
        # prlimit call, or targeting anything other than Runner.Worker, fails.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / "commands"
            scripts = {
                "pgrep": '#!/bin/sh\n[ "$*" = "-o -f Runner.Worker" ] || exit 23\necho 4242\n',
                "sudo": '#!/bin/sh\nprintf "%s\\n" "$*" >> "$MEMLOCK_CALLS"\n[ "$*" = "prlimit --pid 4242 --memlock=unlimited:unlimited" ]\n',
                "prlimit": '#!/bin/sh\n[ "$*" = "--pid 4242 --memlock" ]\n',
            }
            for name, contents in scripts.items():
                path = root / name
                path.write_text(contents)
                path.chmod(0o700)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       MEMLOCK_CALLS=str(commands))
            done = subprocess.run(["bash", "-eo", "pipefail", "-c",
                                   smoke_step("locked memory for released daemon")],
                                  env=env, text=True, capture_output=True, timeout=10)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertEqual(commands.read_text().splitlines(),
                             ["prlimit --pid 4242 --memlock=unlimited:unlimited"])

        inherited = smoke_step("locked memory inherited by smoke")
        for reported, expected in [("unlimited", 0), ("8192", 1)]:
            done = subprocess.run(["bash", "-eo", "pipefail", "-c",
                                   f'ulimit() {{ echo {reported}; }}\n' + inherited],
                                  text=True, capture_output=True, timeout=10)
            self.assertEqual(done.returncode == 0, expected == 0, done.stderr)

    def test_linux_release_smoke_places_store_and_std_on_verified_reflink(self):
        smoke = WORKFLOW.read_text().split("\n  smoke:\n", 1)[1].split("\n  apps:\n", 1)[0]
        swap = "      - name: swap before reflink placement"
        reflink = "      - if: runner.os == 'Linux'\n        uses: ./.github/actions/reflink"
        install = "      - name: install ${{ env.TAG }}"
        self.assertEqual(smoke.count(swap), 1)
        self.assertEqual(smoke.count(reflink), 1)

        def protected(text):
            return swap in text and reflink in text and install in text and (
                text.index(swap) < text.index(reflink) < text.index(install))

        self.assertTrue(protected(smoke))
        # Planted regression: a Linux release tested on ext4 must be rejected
        # before it hits the seeded compiler's strict FICLONE placement.
        self.assertFalse(protected(smoke.replace(reflink, "", 1)))
        shell = smoke_step("swap before reflink placement")
        self.assertIn('[ "$gb" -ge 20 ]', shell)
        self.assertIn('sudo swapon "$swap"', shell)
        preflight = (WORKFLOW.parents[1] / "actions/reflink/action.yml").read_text()
        self.assertIn("FICLONE", preflight)
        self.assertIn("FS_IOC_FIEMAP", preflight)
        self.assertIn("BLAZE_HOME=home", preflight)
        self.assertIn("XDG_CACHE_HOME=cache", preflight)
        self.assertIn('home="${BLAZE_HOME:-$HOME/.cache/blaze}"', smoke)

    def test_pr_smoke_uses_read_token_for_public_corpus(self):
        import json

        smoke = WORKFLOW.read_text().split("\n  smoke:\n", 1)[1].split("\n  apps:\n", 1)[0]
        self.assertEqual(smoke.count("GH_TOKEN: ${{ github.event_name == 'pull_request' && github.token || secrets.BLAZE_BOT_TOKEN }}"), 1)
        self.assertIn('if [ -z "$GH_TOKEN" ]; then', smoke)
        self.assertIn('python3 -I harness/smoke.py', smoke)
        corpus = json.loads((WORKFLOW.parents[2] / "harness/smoke/smoke.json").read_text())
        self.assertTrue(corpus["projects"])
        self.assertFalse([name for name, spec in corpus["projects"].items() if spec.get("private")])


class CorpusTests(unittest.TestCase):
    """The oracle holds the real harness to a historical tree and one selected executable."""
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        self.corpus = self.repository / "harness" / "smoke"
        self.git("init", "-q")
        (self.repository / "README.md").write_text("fixture repository\n")
        self.empty = self.commit("before the corpus")
        self.old_manifest = {
            "cases": {"fixture": {"group": "std", "tests": 1, "args": ["old-argument"]}},
            "projects": {"app": {"group": "apps", "repo": "example/project", "rev": "a" * 40,
                                  "apt": ["old-development"], "brew": ["old-library"],
                                  "fetches": [["--tests"]], "builds": [{"args": ["--workspace"]}],
                                  "runs": [{"args": ["old-project-argument"], "stdout": "project-old\n"}],
                                  "test": []}},
        }
        self.write_case("fixture", "old")
        self.write_manifest(self.old_manifest)
        self.old = self.commit("released corpus")
        self.old_tree = self.git("rev-parse", f"{self.old}:harness/smoke")
        self.current_manifest = json.loads(json.dumps(self.old_manifest))
        self.current_manifest["cases"]["fixture"].update(tests=2, args=["new-argument"])
        self.current_manifest["cases"]["newer"] = {"group": "std", "tests": 1}
        self.current_manifest["projects"]["app"].update(
            rev="b" * 40, apt=["new-development"], brew=["new-library"],
            runs=[{"args": ["new-project-argument"], "stdout": "project-new\n"}])
        self.write_case("fixture", "new")
        self.write_case("newer", "newer")
        self.write_manifest(self.current_manifest)
        self.current = self.commit("newer corpus")
        self.selected = self.root / "selected blaze"
        self.selected.write_text("compiler bytes selected by the caller\n")
        self.args = smoke.Input(groups=("std", "apps"), profiles=("dev",), blaze=str(self.selected),
                                blaze_rev=self.old, corpus_rev=self.old, work=str(self.root / "work"),
                                results=str(self.root / "results.json"))
        self.enterContext(mock.patch.object(smoke, "ROOT", str(self.repository)))
        self.enterContext(mock.patch.object(smoke, "CORPUS", str(self.corpus)))
        self.enterContext(mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}))

    def git(self, *words):
        done = subprocess.run(["git", "-C", str(self.repository), *words], capture_output=True,
                              text=True, check=True, timeout=10)
        return done.stdout.strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("-c", "user.name=Smoke fixture", "-c", "user.email=smoke@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def write_manifest(self, manifest):
        self.corpus.mkdir(parents=True, exist_ok=True)
        (self.corpus / "smoke.json").write_text(json.dumps(manifest))

    def write_case(self, name, value):
        directory = self.corpus / name
        (directory / "src").mkdir(parents=True, exist_ok=True)
        (directory / "Cargo.toml").write_text(f'[package]\nname="{name}"\nversion="0.1.0"\n')
        (directory / "src/main.rs").write_text(f"// {value} source\n")
        (directory / "expected.txt").write_text(value + "\n")

    def exercise(self, corpus, args=None, fail=False, on_version=None):
        calls, fetched = [], []

        def fetch(name, spec, revision, destination):
            self.assertEqual(revision, "a" * 40, "project revision must come from the historical manifest")
            fetched.append((name, revision))
            directory = Path(destination)
            directory.mkdir(parents=True)
            # A workspace pin must not replace the release the caller selected.
            (directory / "Cargo.toml").write_text('[workspace]\n[workspace.metadata.blaze]\nversion = "0.20261105.1"\n')

        def run(argv, cwd, env, timeout):
            calls.append((argv, cwd, timeout))
            self.assertEqual(argv[:2], [str(self.selected), "--ignore-pin"], "selected compiler and pin immunity")
            action = argv[2]
            if action == "--version":
                if on_version is not None:
                    on_version()
                return 0, "blaze fixture\n", "", 0.01
            if action == "stop":
                self.assertEqual(timeout, 120)
                return (None, "", "stop timed out", 120) if fail else (0, "", "", 0.01)
            if action == "slow":
                self.assertEqual(timeout, 300)
                return 23, "", "profile diagnostic failed", 0.01
            if Path(cwd).name == "fixture":
                self.assertEqual((Path(cwd) / "src/main.rs").read_text(), "// old source\n",
                                 "case source must come from the historical snapshot")
                self.assertEqual((Path(cwd) / "expected.txt").read_text(), "old\n",
                                 "expected output must come from the historical snapshot")
                if action == "run":
                    self.assertEqual(argv[3:], ["--", "old-argument"], "case args must come from the historical manifest")
                    return (None, "", "original compiler timeout", 900) if fail else (0, "old\n", "", 0.01)
            elif action == "run":
                self.assertEqual(argv[3:], ["--", "old-project-argument"])
                return 0, "project-old\n", "", 0.01
            if action == "test":
                return 0, "test result: ok. 1 passed; 0 failed;\n", "", 0.01
            self.assertIn(action, {"lock", "build"})
            return 0, "", "", 0.01

        with mock.patch.object(smoke, "run", side_effect=run), mock.patch.object(smoke, "fetch", side_effect=fetch), \
                contextlib.redirect_stdout(io.StringIO()):
            result = smoke.execute(args or self.args, corpus)
        return result, calls, fetched, json.loads(Path((args or self.args).results).read_text())

    def test_historical_manifest_sources_outputs_and_packages_are_one_snapshot(self):
        with smoke.Corpus(self.old) as corpus:
            self.assertEqual(corpus.manifest, self.old_manifest)
            self.assertEqual(corpus.identity["commit"], self.old)
            self.assertEqual(corpus.identity["tree"], self.old_tree)
            self.assertEqual(len(corpus.identity["sha256"]), 64)
            result, calls, fetched, record = self.exercise(corpus)
            self.assertEqual(result, 0)
            self.assertEqual(fetched, [("app", "a" * 40)])
            self.assertEqual({call[0][2] for call in calls}, {"--version", "lock", "build", "run", "test", "stop"})
            self.assertEqual(record["corpus"], corpus.identity)
            self.assertEqual(record["compiler"]["path"], str(self.selected))
            self.assertIsNone(record["compiler"]["engine"], "a standalone selected Blaze remains supported")
            self.assertTrue(all(row["ok"] for row in record["results"]))
            self.assertEqual([row["what"] for row in record["results"] if row.get("skipped")], ["newer"])
            for row in record["results"]:
                if row["repro"]:
                    self.assertIn("--ignore-pin", row["repro"])
                    self.assertIn(str(self.selected), row["repro"])
            for command, package in (("apt", "old-development"), ("brew", "old-library")):
                with contextlib.redirect_stdout(io.StringIO()) as output:
                    code = smoke.execute(replace(self.args, command=command, groups=("apps",)), corpus)
                self.assertEqual(code, 0)
                self.assertEqual(output.getvalue().strip(), package)

    def test_oracle_catches_using_current_case_files_for_a_historical_manifest(self):
        with smoke.Corpus(self.old) as corpus, mock.patch.object(corpus, "root", self.corpus):
            with self.assertRaisesRegex(AssertionError, "historical snapshot"):
                self.exercise(corpus)

    def test_oracle_catches_losing_the_selected_compilers_ignore_pin(self):
        with smoke.Corpus(self.old) as corpus, \
                mock.patch.object(smoke.Smoke, "argv", lambda own, *words: [own.blaze, *words]):
            with self.assertRaisesRegex(AssertionError, "selected compiler and pin immunity"):
                self.exercise(corpus)

    def test_selected_symlink_is_frozen_before_commands_and_shutdown(self):
        alias = self.root / "selected-link"
        alias.symlink_to(self.selected)
        replacement = self.root / "another-compiler"
        replacement.write_text("another compiler\n")

        def retarget():
            alias.unlink()
            alias.symlink_to(replacement)

        args = replace(self.args, blaze=str(alias))
        with smoke.Corpus(self.old) as corpus:
            result, calls, _, record = self.exercise(corpus, args, on_version=retarget)
        self.assertEqual(result, 0)
        self.assertTrue(all(argv[0] == str(self.selected) for argv, _, _ in calls))
        self.assertEqual(record["compiler"]["path"], str(self.selected))
        # Plant the original abspath-only selection; the same command oracle must
        # see the unfrozen alias before any subprocess can select another build.
        alias.unlink()
        alias.symlink_to(self.selected)
        with smoke.Corpus(self.old) as corpus, mock.patch.object(smoke, "blaze_program", os.path.abspath):
            with self.assertRaisesRegex(AssertionError, "selected compiler and pin immunity"):
                self.exercise(corpus, args, on_version=retarget)

    def test_companion_drift_is_required_by_the_identity_oracle(self):
        companion = self.selected.with_name("blaze-engine" + smoke.EXE)
        original_identity = smoke.compiler_identity

        def assert_detected(suffix):
            companion.write_text("original companion\n")
            args = replace(self.args, work=str(self.root / f"work-{suffix}"),
                           results=str(self.root / f"results-{suffix}.json"))
            with smoke.Corpus(self.old) as corpus:
                result, _, _, record = self.exercise(
                    corpus, args, on_version=lambda: companion.write_text("changed companion\n"))
            self.assertEqual(result, 1, "companion mutation must fail the run")
            self.assertTrue(any(row["what"] == "compiler identity" and not row["ok"]
                                for row in record["results"]))

        assert_detected("healthy")

        def client_only(program):
            identity = original_identity(program)
            identity["engine"] = None
            return identity

        with mock.patch.object(smoke, "compiler_identity", side_effect=client_only):
            with self.assertRaisesRegex(AssertionError, "companion mutation"):
                assert_detected("planted")

    def test_companion_appearance_removal_and_retargeting_change_identity(self):
        companion = self.selected.with_name("blaze-engine" + smoke.EXE)
        absent = smoke.compiler_identity(str(self.selected))
        first, second = self.root / "first-engine", self.root / "second-engine"
        first.write_text("engine bytes\n")
        second.write_text("engine bytes\n")
        companion.symlink_to(first)
        present = smoke.compiler_identity(str(self.selected))
        self.assertNotEqual(present, absent)
        companion.unlink()
        companion.symlink_to(second)
        self.assertNotEqual(smoke.compiler_identity(str(self.selected)), present,
                            "the same bytes at another engine path still changed the selected executable set")
        companion.unlink()
        self.assertEqual(smoke.compiler_identity(str(self.selected)), absent)

    def test_original_timeout_and_failed_bounded_shutdown_are_both_retained(self):
        args = replace(self.args, groups=("std",), only=("fixture",))
        with smoke.Corpus(self.old) as corpus:
            result, calls, _, record = self.exercise(corpus, args, fail=True)
        self.assertEqual(result, 1)
        original = next(row for row in record["results"] if row["what"] == "fixture dev run")
        self.assertFalse(original["ok"])
        for cause in ("original compiler timeout", "profile diagnostic failed", "stop timed out"):
            self.assertIn(cause, original["stderr"])
        stopped = next(row for row in record["results"] if row["what"] == "daemon stop")
        self.assertFalse(stopped["ok"])
        self.assertIn("stop timed out", stopped["stderr"])
        self.assertEqual([call[0][2] for call in calls].count("slow"), 1)
        self.assertEqual([call[0][2] for call in calls].count("stop"), 2)

    def test_missing_revision_corpus_and_case_inputs_refuse_instead_of_skipping(self):
        for revision in ("no-such-revision", self.empty):
            with self.subTest(revision=revision), self.assertRaises(ValueError):
                smoke.Corpus(revision)
        (self.corpus / "fixture" / "expected.txt").unlink()
        incomplete = self.commit("missing expected result")
        with self.assertRaisesRegex(ValueError, "lacks expected.txt"):
            smoke.Corpus(incomplete)
        (self.corpus / "smoke.json").unlink()
        missing_manifest = self.commit("missing corpus manifest")
        with self.assertRaises(FileNotFoundError):
            smoke.Corpus(missing_manifest)

    def test_newer_only_selection_and_empty_profiles_do_not_pass_without_work(self):
        with smoke.Corpus(self.old) as corpus:
            with self.assertRaisesRegex(ValueError, "no cases or projects to run"):
                smoke.selection(replace(self.args, only=("newer",)), corpus)
        for value in ({"profiles": []}, {"groups": []}, {"command": "cargo"}, {"unknown": True}):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                smoke.typed(json.dumps(value))

    def test_archive_paths_links_and_resource_limits_are_refused(self):
        def archive(name, kind=tarfile.REGTYPE, payload=b"fixture"):
            data = io.BytesIO()
            with tarfile.open(fileobj=data, mode="w:") as contents:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.size = len(payload) if kind == tarfile.REGTYPE else 0
                member.linkname = "../../outside"
                contents.addfile(member, io.BytesIO(payload) if kind == tarfile.REGTYPE else None)
            data.seek(0)
            return data

        corpus = object.__new__(smoke.Corpus)
        corpus.root = self.root / "extraction"
        corpus.root.mkdir()
        for name, kind in (("../outside", tarfile.REGTYPE), ("/outside", tarfile.REGTYPE),
                           ("fixture/link", tarfile.SYMTYPE), ("fixture/hard", tarfile.LNKTYPE)):
            with self.subTest(name=name), self.assertRaises(ValueError):
                corpus.extract(archive(name, kind))
        for limit in ("CORPUS_BYTES", "CORPUS_FILES"):
            with self.subTest(limit=limit), mock.patch.object(smoke, limit, 0), self.assertRaisesRegex(ValueError, "exceeds"):
                corpus.extract(archive("fixture"))
        self.assertFalse((self.root / "outside").exists())

    @unittest.skipUnless(os.name == "posix", "release smoke acquisition runs on Linux and macOS")
    def test_oracle_catches_an_archive_bound_enforced_only_after_acquisition(self):
        # Even this small fixture's tar exceeds one block. The source-reading Git
        # child must never write more than the bound, even on its failure path.
        def bounded():
            with tempfile.TemporaryFile() as output:
                with self.assertRaises(ValueError):
                    smoke.git("archive", "--format=tar", f"{self.old}:harness/smoke", output=output,
                              max_bytes=512)
                self.assertLessEqual(output.tell(), 512, "archive must be bounded before disk acquisition")

        bounded()
        with mock.patch.object(smoke, "archive_limit", lambda _maximum: None):
            with self.assertRaisesRegex(AssertionError, "before disk acquisition"):
                bounded()

    def test_selected_projects_must_contain_an_executable_check(self):
        for empty in ({"fetches": [["--tests"]]}, {"builds": [], "runs": []},
                      {"test": None}, {"runs": {}}, {"builds": ["not a step"]},
                      {"builds": [{}], "macos": {"builds": []}},
                      {"builds": [{}], "macos": []}):
            with self.subTest(spec=empty):
                manifest = json.loads(json.dumps(self.old_manifest))
                manifest["projects"]["app"] = dict(group="apps", repo="example/project", rev="a" * 40,
                                                    **empty)
                self.write_manifest(manifest)
                revision = self.commit("incomplete project check")
                with self.assertRaises(ValueError):
                    smoke.Corpus(revision)
        # An explicitly empty test argument list still invokes `blaze test` and
        # its real-output oracle requires at least one passing test.
        manifest["projects"]["app"] = dict(group="apps", repo="example/project", rev="a" * 40, test=[])
        self.write_manifest(manifest)
        accepted = self.commit("test-only project")
        with smoke.Corpus(accepted) as corpus:
            self.assertEqual(corpus.manifest["projects"]["app"]["test"], [])

    def test_oracle_catches_accepting_a_fetch_only_project(self):
        manifest = json.loads(json.dumps(self.old_manifest))
        manifest["projects"]["app"] = dict(group="apps", repo="example/project", rev="a" * 40,
                                            fetches=[["--tests"]])
        self.write_manifest(manifest)
        revision = self.commit("fetch-only project")
        args = replace(self.args, groups=("apps",), only=("app",), corpus_rev=revision)

        def refused():
            try:
                with smoke.Corpus(revision) as corpus:
                    result, _, _, _ = self.exercise(corpus, args)
            except ValueError as error:
                self.assertIn("no build, run or test check", str(error))
                return
            self.assertNotEqual(result, 0, "fetch-only preparation cannot establish a compiler pass")

        refused()
        with mock.patch.object(smoke, "project_checks", lambda *_args: None):
            with self.assertRaisesRegex(AssertionError, "fetch-only preparation"):
                refused()

    def release_action(self, transform=lambda body: body, revision=None):
        action = WORKFLOW.parents[1] / "actions" / "release-corpus" / "action.yml"
        lines = action.read_text().splitlines()
        self.assertEqual(lines.count("      run: |"), 1)
        body = "\n".join(line[8:] for line in lines[lines.index("      run: |") + 1:]) + "\n"
        directory = Path(tempfile.mkdtemp(dir=self.root))
        checkout = directory / "checkout"
        subprocess.run(["git", "clone", "-q", "--depth=1", self.repository.as_uri(), str(checkout)],
                       check=True, capture_output=True, text=True, timeout=10)

        def git(*words):
            return subprocess.run(["git", "-C", str(checkout), *words], check=True, text=True,
                                  capture_output=True, timeout=10).stdout.strip()

        # The actual action's fetch resolves to a local fixture; an unexpected remote is refused.
        git("config", "protocol.https.allow", "never")
        git("config", "protocol.file.allow", "always")
        git("config", f"url.{self.repository.as_uri()}.insteadOf", "https://github.com/example/release.git")
        before = git("rev-parse", "HEAD")
        output = directory / "outputs"
        env = dict(os.environ, CORPUS_COMMIT=revision or self.old, CORPUS_REPOSITORY="example/release",
                   RUNNER_TEMP=str(directory), GITHUB_OUTPUT=str(output))
        env.pop("BASH_ENV", None)
        done = subprocess.run(["bash", "-eo", "pipefail", "-c", transform(body)], cwd=checkout,
                              env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(git("rev-parse", "HEAD"), before, "fetching the corpus must not replace the tested checkout")
        manifest = None
        if output.exists():
            fields = dict(line.split("=", 1) for line in output.read_text().splitlines())
            manifest = json.loads(Path(fields["manifest"]).read_text())
        return done, manifest

    def test_release_action_fetches_the_exact_missing_commit_without_switching_checkout(self):
        done, manifest = self.release_action()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(manifest, self.old_manifest)
        done, manifest = self.release_action(revision=self.empty)
        self.assertNotEqual(done.returncode, 0)
        self.assertIsNone(manifest)

    def test_oracle_catches_release_action_reading_heads_manifest(self):
        done, manifest = self.release_action(lambda body: body.replace(
            'git show "$CORPUS_COMMIT:harness/smoke/smoke.json"',
            'git show "HEAD:harness/smoke/smoke.json"'))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        with self.assertRaises(AssertionError):
            self.assertEqual(manifest, self.old_manifest)

    def test_workflow_package_queries_keep_revision_and_propagate_query_failure(self):
        double = '''import json, os, pathlib, sys
assert sys.argv[1:3] == ["-I", "harness/smoke.py"], sys.argv
assert len(sys.argv) == 4, sys.argv
given = json.loads(pathlib.Path(sys.argv[3]).read_text())
with pathlib.Path(os.environ["QUERY_CALLS"]).open("a") as out:
    out.write(json.dumps(given) + "\\n")
if os.environ.get("QUERY_FAIL") == "yes":
    sys.exit(29)
print("fixture-library")
'''
        for step, system in (("what the projects build with", "Linux"),
                             ("what ${{ matrix.app }} builds with", "Linux"),
                             ("what ${{ matrix.app }} builds with", "macOS")):
            for failed in (False, True):
                with self.subTest(step=step, system=system, failed=failed):
                    directory = Path(tempfile.mkdtemp(dir=self.root))
                    binary = directory / "bin"
                    binary.mkdir()
                    implementation = directory / "query.py"
                    implementation.write_text(double)
                    (binary / "python3").write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable)
                                                  + " -I " + shlex.quote(str(implementation)) + ' "$@"\n')
                    for name in ("sudo", "brew"):
                        (binary / name).write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$PACKAGE_CALLS"\n')
                    for path in binary.iterdir():
                        path.chmod(0o700)
                    queries, packages = directory / "queries", directory / "packages"
                    env = dict(os.environ, PATH=str(binary) + os.pathsep + os.environ["PATH"],
                               RUNNER_TEMP=str(directory), CORPUS_COMMIT=self.old, RUNNER_OS=system, APP="app",
                               QUERY_CALLS=str(queries), PACKAGE_CALLS=str(packages), QUERY_FAIL="yes" if failed else "no")
                    env.pop("BASH_ENV", None)
                    done = subprocess.run(["bash", "-eo", "pipefail", "-c", smoke_step(step)],
                                          cwd=self.repository, env=env, text=True, capture_output=True, timeout=10)
                    self.assertEqual(done.returncode, 29 if failed else 0, done.stdout + done.stderr)
                    given = json.loads(queries.read_text())
                    self.assertEqual(given["command"], "apt" if system == "Linux" else "brew")
                    self.assertEqual(given["corpus_rev"], self.old)
                    if "matrix.app" in step:
                        self.assertEqual(given["groups"], ["apps"])
                        self.assertEqual(given["only"], ["app"])
                    self.assertEqual(packages.exists(), not failed)
                    if not failed:
                        self.assertIn("fixture-library", packages.read_text())


if __name__ == "__main__":
    unittest.main()
