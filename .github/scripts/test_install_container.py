#!/usr/bin/env python3
"""Exercise the actual installer identity preflight without Docker or a compiler."""

from dataclasses import replace
import importlib.util
import json
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("install_container.py")
SPEC = importlib.util.spec_from_file_location("install_container", SCRIPT)
assert SPEC and SPEC.loader
container = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = container
SPEC.loader.exec_module(container)


DOUBLE = r'''
import json, os, pathlib, sys, tempfile
root = pathlib.Path(__file__).parent.parent
config = json.loads((root / "config.json").read_text())
name = pathlib.Path(__file__).stem
if name == "id":
    assert sys.argv[1:] in (["-u"], ["-g"])
    print(0 if config.get("root_user") else 1001)
elif name == "getent":
    assert sys.argv[1:] == ["passwd", "1001"]
    (root / "lookup-env.json").write_text(json.dumps(dict(os.environ)))
    if config.get("missing_user"):
        sys.exit(2)
    home = "/wrong/home" if config.get("wrong_home") else str(root / "home")
    gid = 1002 if config.get("wrong_gid") else 1001
    print(f"blaze:x:1001:{gid}:Blaze:{home}:/bin/sh")
elif name == "mktemp":
    (root / "probe-attempt").touch()
    if config.get("cache_denied"):
        print("cache: Permission denied", file=sys.stderr)
        sys.exit(1)
    assert len(sys.argv) == 2
    fd, path = tempfile.mkstemp(prefix="blaze-install.", dir=pathlib.Path(sys.argv[1]).parent)
    os.close(fd)
    print(path)
else:
    sys.exit(99)
'''


def options(cmd, flag):
    return [cmd[i + 1] for i, arg in enumerate(cmd[:-1]) if arg == flag]


class ContainerIdentityTests(unittest.TestCase):
    def exercise(self, **config):
        with tempfile.TemporaryDirectory(prefix="blaze identity ") as directory:
            root = Path(directory)
            (root / "bin").mkdir()
            (root / "home/.cache").mkdir(parents=True)
            (root / "config.json").write_text(json.dumps(config))
            for name in ("id", "getent", "mktemp"):
                implementation = root / "bin" / (name + ".py")
                implementation.write_text(DOUBLE)
                command = root / "bin" / name
                command.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I -S "
                                   + shlex.quote(str(implementation)) + ' "$@"\n')
                command.chmod(0o700)
            env = {"PATH": str(root / "bin") + ":/usr/bin:/bin", "HOME": str(root / "home"),
                   "XDG_CACHE_HOME": str(root / "unrelated-cache"), "IDENTITY_MUST_NOT_INHERIT": "set"}
            result = subprocess.run(["/bin/sh", "-eu", "-c", container.PREFLIGHT], cwd=root, env=env,
                                    text=True, capture_output=True, timeout=30)
            lookup = root / "lookup-env.json"
            lookup_env = json.loads(lookup.read_text()) if lookup.exists() else None
            return result, lookup_env, (root / "probe-attempt").exists(), list((root / "home/.cache").iterdir())

    def test_passwd_home_and_cache_work_without_inherited_environment(self):
        # The invariant: the launcher's account resolves with no environment and its home
        # cache is actually writable before either installer recipe starts Blaze.
        result, lookup_env, probed, remaining = self.exercise()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIsNotNone(lookup_env)
        # The executable double's shebang shell may create its own PATH or PWD.
        # None of the parent values that could mask a missing passwd entry may survive.
        for key in ("HOME", "XDG_CACHE_HOME", "IDENTITY_MUST_NOT_INHERIT"):
            self.assertNotIn(key, lookup_env)
        self.assertTrue(probed)
        self.assertEqual(remaining, [])

    def test_the_oracle_catches_missing_account_wrong_home_and_denied_cache(self):
        controls = {"missing_user": "no passwd entry", "wrong_home": "does not match",
                    "wrong_gid": "does not match", "cache_denied": "cannot write",
                    "root_user": "non-root user and group"}
        for control, says in controls.items():
            with self.subTest(control=control):
                result, _, probed, remaining = self.exercise(**{control: True})
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(says, result.stdout + result.stderr)
                self.assertEqual(probed, control == "cache_denied")
                self.assertEqual(remaining, [])

    def test_dockerfile_registers_matching_ids_and_keeps_the_home_literal(self):
        user = container.User(uid=2002, gid=2003, home="/work with spaces/$literal'\"")
        for create_home in (True, False):
            with self.subTest(create_home=create_home):
                commands = [json.loads(line.removeprefix("RUN "))
                            for line in user.dockerfile(create_home=create_home).splitlines()]
                self.assertEqual(commands[0], ["groupadd", "--gid", "2003", "blaze"])
                self.assertEqual(commands[1][0], "useradd")
                self.assertEqual(options(commands[1], "--uid"), ["2002"])
                self.assertEqual(options(commands[1], "--gid"), ["2003"])
                self.assertEqual(options(commands[1], "--home-dir"), [user.home])
                self.assertIn("--create-home" if create_home else "--no-create-home", commands[1])
                self.assertIn("--no-log-init", commands[1])
                self.assertEqual(user.ids, "2002:2003")

    def test_root_invalid_ids_and_passwd_delimiters_are_refused(self):
        user = container.User(uid=1001, gid=1001, home="/home/blaze")
        for changes in ({"uid": 0}, {"gid": 0}, {"uid": -1}, {"gid": 2**32 - 1},
                        {"uid": True}, {"home": "relative"}, {"home": "/home/../root"},
                        {"home": "/home/name:field"}, {"home": "/home/name\nentry"}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                replace(user, **changes)


if __name__ == "__main__":
    unittest.main()
