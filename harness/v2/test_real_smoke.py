"""real_smoke.py's own wiring. Synthetic executables are never run; no Blaze process is started.

The controls hold the harness to what it promises: the D52 request it makes for the proc-macro
and the build-script modules (the lock's own checkout, the sandbox required, the exact feature
sets), the records it copies into a fresh home, the pinned C toolchain's shelf it copies there
beside the Rust one and refuses when a member is missing or differs, the blaze-edition member's
manifest contract, and the input it refuses. A planted bug (an unsandboxed request, a missing module, a manifest
that still names bstd) fails its control.
"""

import contextlib
import io
import json
import shutil
import subprocess
import tempfile
import typing
import unittest
from collections.abc import Mapping
from dataclasses import asdict, replace
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

import real_smoke as rs


class RealSmokeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.enterContext(patch.object(rs.platform, "system", return_value="Darwin"))
        self.enterContext(patch.object(rs.platform, "machine", return_value="arm64"))
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            path = self.bin / name
            path.write_text(f"synthetic {name}; never executed\n")
            path.chmod(0o700)
        self.builder = self.root / "proc_macro_modules"
        self.builder.write_text("synthetic module builder; never executed\n")
        self.builder.chmod(0o700)
        self.shelf = self.root / "shelf"
        self.shelf.mkdir()
        (self.shelf / "manifest-record").write_text("synthetic shelf\n")
        self.toolchain_home = self.root / "toolchain-home"
        (self.toolchain_home / ".rustup" / "toolchains").mkdir(parents=True)
        self.clang = self.shelf / "clang"
        self.pin = typing.cast(dict[str, Any], json.loads((rs.TREE / "pins/clang.json").read_text()))
        self.clang_digest = typing.cast(
            rs.ClangPin, typing.cast(dict[str, Any], self.pin["targets"])[rs.HOST]
        )["sha256"]
        self.programs = {
            name: typing.cast(rs.Program, value)
            for name, value in typing.cast(dict[str, Any], self.pin["shelf_programs"])[rs.HOST].items()
            if isinstance(value, dict)
        }
        self.make_clang_shelf()
        self.given = rs.Input(
            str(self.bin),
            str(self.root / "work"),
            "",
            modules=str(self.builder),
            toolchain_home=str(self.toolchain_home),
            shelf=str(self.shelf),
            clang_shelf=str(self.clang),
        )

    def make_clang_shelf(self) -> Path:
        """A synthetic C toolchain's shelf under `self.clang`, named by the pin's archive digest and
        holding every member the native runs start or load (none is executed)."""
        shelf = self.clang / self.clang_digest
        for member in rs.CLANG_MEMBERS:
            (shelf / member).parent.mkdir(parents=True, exist_ok=True)
            (shelf / member).write_text(f"synthetic {member}; never executed\n")
        return shelf

    def digests(self, differ: dict[str, str] | None = None) -> Any:
        """`hashlib.file_digest` as the harness reads it: a program's file hashes to its row's
        SHA-256, so the shelf is what the pin table rows, except for a path in `differ`."""
        rows = {
            str(self.clang / self.clang_digest / program["member"].split("/", 1)[1]): program["sha256"]
            for program in self.programs.values()
        }
        rows.update(differ or {})

        def file_digest(held: Any, algorithm: str) -> Any:
            self.assertEqual(algorithm, "sha256")
            digest = rows[held.name]
            return SimpleNamespace(hexdigest=lambda: digest)

        return patch.object(rs.hashlib, "file_digest", side_effect=file_digest)

    def invoke(
        self, given: rs.Input | None = None, code: int = 1, differ: dict[str, str] | None = None
    ) -> None:
        given = given or self.given
        with (
            self.digests(differ),
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaises(SystemExit) as ended,
        ):
            rs.main(["real_smoke.py", json.dumps(asdict(given))])
        self.assertEqual(ended.exception.code, code)

    def fake_builder(self, made: dict[str, int]) -> tuple[list[dict[str, Any]], list[dict[str, str]], Any]:
        """A module builder that writes `made[kind]` module records under the store it is given
        and records each request and environment it was handed."""
        requests: list[dict[str, Any]] = []
        environments: list[dict[str, str]] = []

        def run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
            self.assertEqual(Path(argv[0]), self.builder)
            request = json.loads(argv[1])
            requests.append(request)
            environments.append(kwargs["env"])
            inner = request.get("build_scripts", request)
            kind = "build-script" if "build_scripts" in request else "proc-macro"
            for at in range(made.get(kind, 0)):
                record = Path(inner["store"]) / kind / "modules" / f"{at:064x}"
                record.mkdir(parents=True)
                (record / "module.wasm").write_text(f"synthetic {kind} module {at}\n")
                (record / "identity").write_text(f"name synthetic{at}\n")
            return subprocess.CompletedProcess(argv, 0, "", "")

        return requests, environments, patch.object(rs.subprocess, "run", side_effect=run)

    def test_modules_are_asked_of_the_lock_with_the_sandbox_and_copied_into_the_home(self) -> None:
        requests, environments, builder = self.fake_builder(
            {"proc-macro": len(rs.MACROS), "build-script": len(rs.SCRIPTS)}
        )
        rows: list[tuple[str, Mapping[str, str], rs.Proof, int]] = []

        def ran(command: rs.Command, proof: rs.Proof) -> rs.ran.Ran:
            self.assertEqual(command.args[1:3], ("run", "-p"))
            rows.append((command.args[3], command.env, proof, command.exit))
            return rs.ran.Ran(1.0, 0, proof.stdout, "")

        with builder, patch.object(rs.ran, "run", side_effect=ran):
            self.invoke(code=0)
        self.assertEqual([("build_scripts" in it) for it in requests], [False, True])
        for request in requests:
            inner = request.get("build_scripts", request)
            self.assertEqual(Path(inner["checkout"]), self.root / "work" / "p")
            self.assertEqual(Path(inner["tree"]), rs.TREE)
            self.assertIs(inner["unsandboxed_build_scripts"], False)
            self.assertNotIn("vendor_from", inner)
        self.assertEqual(requests[0]["crates"], list(rs.MACROS))
        self.assertEqual(requests[1]["build_scripts"]["crates"], list(rs.SCRIPTS))
        for environment in environments:
            self.assertEqual(
                environment,
                {"HOME": str(self.toolchain_home), "PATH": self.given.path, "RUST_BACKTRACE": "0"},
            )
        home = self.root / "work" / "home" / ".blaze" / "v2"
        self.assertEqual(len(list((home / "proc-macro" / "modules").glob("*/module.wasm"))), len(rs.MACROS))
        self.assertEqual(
            len(list((home / "build-script" / "modules").glob("*/module.wasm"))), len(rs.SCRIPTS)
        )
        self.assertTrue((home / "toolchain" / "manifest-record").is_file())
        self.assertEqual([name for name, _, _, _ in rows], ["smoke", "smoke", "smoke", "smoke-edition"])
        self.assertEqual([proof.stdout for _, _, proof, _ in rows], [rs.line(1), rs.line(1), rs.line(7), rs.EDITION])
        for _, env, _, status in rows:
            self.assertEqual(status, 0)
            self.assertEqual(env["HOME"], str(self.root / "work" / "home"))
            self.assertEqual(env["RUST_BACKTRACE"], "0")
            self.assertNotIn("CARGO_TARGET_DIR", env)
        for step in ("cold", "noop", "edit", "edition"):
            self.assertTrue((self.root / "work" / "logs" / f"{step}.stdout").is_file())

    def test_a_failed_or_short_module_build_is_invalid_and_nothing_is_copied(self) -> None:
        made = {"proc-macro": len(rs.MACROS), "build-script": len(rs.SCRIPTS) - 1}
        _, _, builder = self.fake_builder(made)
        with builder, patch.object(rs.ran, "run") as run:
            self.invoke(code=1)
        run.assert_not_called()
        self.assertFalse((self.root / "work" / "home" / ".blaze" / "v2" / "build-script").exists())

    def test_without_a_builder_no_module_is_asked_for(self) -> None:
        requests, _, builder = self.fake_builder({})
        with (
            builder,
            patch.object(
                rs.ran, "run", return_value=rs.ran.Ran(1.0, 125, "", "refused", "exit 125, not 0")
            ) as run,
        ):
            self.invoke(replace(self.given, modules=""), code=1)
        self.assertEqual(requests, [])
        self.assertEqual(run.call_count, 4)

    def test_the_c_toolchain_shelf_is_copied_into_the_home_beside_the_rust_one(self) -> None:
        requests, _, builder = self.fake_builder(
            {"proc-macro": len(rs.MACROS), "build-script": len(rs.SCRIPTS)}
        )

        def ran(command: rs.Command, proof: rs.Proof) -> rs.ran.Ran:
            return rs.ran.Ran(1.0, 0, proof.stdout, "")

        with builder, patch.object(rs.ran, "run", side_effect=ran):
            self.invoke(code=0)
        self.assertEqual(len(requests), 2)
        home = self.root / "work" / "home" / ".blaze" / "v2" / "toolchain"
        self.assertEqual((home / "manifest-record").read_text(), "synthetic shelf\n")
        for member in rs.CLANG_MEMBERS:
            self.assertTrue((home / "clang" / self.clang_digest / member).is_file(), member)

    def refuse(self, said: str, differ: dict[str, str] | None = None) -> None:
        """`main` with `self.given` stops before any row runs, saying `said`."""
        with self.digests(differ), self.assertRaisesRegex(SystemExit, said):
            rs.main(["real_smoke.py", json.dumps(asdict(self.given))])
        self.assertFalse((self.root / "work").exists())

    def test_the_full_smoke_refuses_a_host_without_the_pinned_c_backend(self) -> None:
        with patch.object(rs.platform, "system", return_value="Linux"):
            self.refuse("require the macOS ARM64 pinned native C backend")

    def test_a_partial_stdout_line_cannot_satisfy_a_row(self) -> None:
        with patch.object(rs.ran, "run", return_value=rs.ran.Ran(1.0, 0, f"extra {rs.line(1)}\n", "")):
            self.invoke(replace(self.given, modules="", steps=("cold",)), code=1)

    def test_a_c_toolchain_shelf_that_lacks_a_member_or_differs_is_refused_by_name(self) -> None:
        shelf = self.clang / self.clang_digest
        (shelf / "bin/llvm-nm").unlink()
        self.refuse("holds no `bin/llvm-nm`")
        self.make_clang_shelf()
        name, program = next(iter(self.programs.items()))
        path = shelf / program["member"].split("/", 1)[1]
        self.refuse(f"is not `{name}`", differ={str(path): "0" * 64})
        shutil.rmtree(self.clang / self.clang_digest)
        self.refuse("is not unpacked there")
        self.make_clang_shelf()
        # Whole again: the check passes and the rows run (and refuse: no Blaze process is real).
        requests, _, builder = self.fake_builder({})
        with (
            builder,
            patch.object(
                rs.ran, "run", return_value=rs.ran.Ran(1.0, 125, "", "refused", "exit 125, not 0")
            ) as run,
        ):
            self.invoke(replace(self.given, modules=""), code=1)
        self.assertEqual(requests, [])
        self.assertEqual(run.call_count, 4)

    def test_the_edition_member_names_nothing_of_its_standard_library(self) -> None:
        manifest = (rs.SOURCE / "edition" / "Cargo.toml").read_text()
        self.assertIn('edition = "blaze-2026.11"', manifest)
        self.assertNotIn("bstd", manifest.split("[package]")[1])
        self.assertNotIn("blaze-std", manifest.split("[package]")[1])
        main = (rs.SOURCE / "edition" / "src" / "main.rs").read_text()
        self.assertIn("fn main(cx: &Cx, out: Out) -> Result<(), Error>", main)
        self.assertNotIn("io: Io", main)
        self.assertIn(rs.EDITION, main)

    def test_the_lock_holds_the_workspace_roots_and_no_bstd_copy(self) -> None:
        lock = (rs.SOURCE / "Cargo.lock").read_text()
        self.assertIn('name = "smoke-edition"\nversion = "0.1.0"\n\n', lock)
        self.assertNotIn("blaze-std", lock)
        for crate in (*rs.MACROS, *rs.SCRIPTS):
            self.assertIn(f'name = "{crate["name"]}"\nversion = "{crate["version"]}"\n', lock)

    def test_the_input_is_refused(self) -> None:
        base = {"blaze": str(self.bin), "work": str(self.root / "w"), "tmpfs": ""}
        for name, value, said in (
            ("bogus", 1, "no field bogus"),
            ("workers", 0, "positive"),
            ("workers", "4", "is a int"),
            ("steps", ["edit"], "needs `cold`"),
            ("steps", ["noop", "cold"], "in that order"),
            ("modules", str(self.root / "absent"), "not an executable"),
            ("toolchain_home", str(self.root / "absent"), "holds no .rustup/toolchains"),
        ):
            with self.subTest(name=name, value=value), self.assertRaisesRegex(SystemExit, said):
                rs.main(["real_smoke.py", json.dumps({**base, name: value})])
        with self.assertRaisesRegex(SystemExit, "toolchain_home is needed with modules"):
            rs.main(["real_smoke.py", json.dumps({**base, "modules": str(self.builder)})])
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            with self.subTest(name=name):
                path = self.bin / name
                path.unlink()
                with self.assertRaisesRegex(SystemExit, f"holds no `{name}`"):
                    rs.main(["real_smoke.py", json.dumps(base)])
                path.write_text("synthetic\n")
                path.chmod(0o700)


if __name__ == "__main__":
    unittest.main()
