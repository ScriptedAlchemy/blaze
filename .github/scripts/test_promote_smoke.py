#!/usr/bin/env python3
"""Exercise promote's real shell fixture with a V2 CLI that deliberately lacks 'new'."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "promote.yaml"


def smoke_step():
    lines = WORKFLOW.read_text().splitlines()
    heading = "      - name: what it installed runs"
    if lines.count(heading) != 1:
        raise AssertionError("promote needs exactly one installed-release smoke step")
    at = lines.index(heading) + 1
    if lines[at] != "        run: |":
        raise AssertionError("installed-release smoke must have a shell script")
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


if __name__ == "__main__":
    unittest.main()
