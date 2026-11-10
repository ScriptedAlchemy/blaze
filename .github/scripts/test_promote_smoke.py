#!/usr/bin/env python3
"""Exercise promote's real shell fixture with a V2 CLI that deliberately lacks 'new'."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "promote.yaml"


def smoke_step(name="what it installed runs"):
    lines = WORKFLOW.read_text().splitlines()
    heading = f"      - name: {name}"
    if lines.count(heading) != 1:
        raise AssertionError(f"promote needs exactly one {name} smoke step")
    at = lines.index(heading) + 1
    if lines[at] == "        if: runner.os == 'Linux'":
        at += 1
    if lines[at] != "        run: |":
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



if __name__ == "__main__":
    unittest.main()
