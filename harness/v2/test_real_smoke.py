"""real_smoke.py's own wiring. Synthetic executables are never run; no Blaze process is started.

The controls hold the harness to what it promises: the D52 request it makes for the proc-macro
and the build-script modules (the lock's own checkout, the sandbox required, the exact feature
sets), the records it copies into a fresh home, the blaze-edition member's manifest contract,
and the input it refuses. A planted bug (an unsandboxed request, a missing module, a manifest
that still names bstd) fails its control.
"""

import contextlib
import io
import json
import subprocess
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from typing import Any
from unittest.mock import patch

import real_smoke as rs


class RealSmokeTests(unittest.TestCase):
    def setUp(self) -> None:
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
        self.given = rs.Input(
            str(self.bin),
            str(self.root / "work"),
            "",
            modules=str(self.builder),
            toolchain_home=str(self.toolchain_home),
            shelf=str(self.shelf),
        )

    def invoke(self, given: rs.Input | None = None, code: int = 1) -> None:
        given = given or self.given
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as ended:
            rs.main(
                [
                    "real_smoke.py",
                    json.dumps(
                        {
                            "blaze": given.blaze,
                            "work": given.work,
                            "tmpfs": given.tmpfs,
                            "modules": given.modules,
                            "toolchain_home": given.toolchain_home,
                            "shelf": given.shelf,
                        }
                    ),
                ]
            )
        self.assertEqual(ended.exception.code, code)

    def fake_builder(self, made: dict[str, int]) -> tuple[list[dict[str, Any]], list[dict[str, str]], Any]:
        """A module builder that writes `made[kind]` module records under the store it is given
        and records each request and environment it was handed."""
        requests: list[dict[str, Any]] = []
        environments: list[dict[str, str]] = []

        def run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
            if Path(argv[0]) != self.builder:
                # `blaze daemon --stop`: never started; the harness only reads its exit status.
                return subprocess.CompletedProcess(argv, 0, "", "")
            request = json.loads(argv[1])
            requests.append(request)
            environments.append(kwargs["env"])
            inner = request["build_scripts"] if "build_scripts" in request else request
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
        rows: list[tuple[str, dict[str, str]]] = []

        def ran(command: rs.Command, proof: rs.Proof) -> rs.ran.Ran:
            rows.append((command.args[3], command.env))
            return rs.ran.Ran(1.0, 0, proof.stdout, "")

        with builder, patch.object(rs.ran, "run", side_effect=ran):
            self.invoke(code=0)
        self.assertEqual([("build_scripts" in it) for it in requests], [False, True])
        for request in requests:
            inner = request["build_scripts"] if "build_scripts" in request else request
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
        self.assertEqual([name for name, _ in rows], ["smoke", "smoke", "smoke", "smoke-edition"])
        for _, env in rows:
            self.assertEqual(env["HOME"], str(self.root / "work" / "home"))
            self.assertEqual(env["RUST_BACKTRACE"], "0")
            self.assertNotIn("CARGO_TARGET_DIR", env)
        for step in ("cold", "noop", "edit", "edition"):
            self.assertTrue((self.root / "work" / "logs" / f"{step}.stdout").is_file())

    def test_a_failed_or_short_module_build_is_invalid_and_nothing_is_copied(self) -> None:
        for made, code in (({"proc-macro": len(rs.MACROS), "build-script": len(rs.SCRIPTS) - 1}, 1),):
            with self.subTest(made=made):
                requests, environments, builder = self.fake_builder(made)
                out = io.StringIO()
                with builder, contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as ended:
                    rs.main(
                        [
                            "real_smoke.py",
                            json.dumps(
                                {
                                    "blaze": str(self.bin),
                                    "work": str(self.root / f"w{code}"),
                                    "tmpfs": "",
                                    "modules": str(self.builder),
                                    "toolchain_home": str(self.toolchain_home),
                                    "shelf": str(self.shelf),
                                }
                            ),
                        ]
                    )
                self.assertEqual(ended.exception.code, 1)
                self.assertIn("INVALID", out.getvalue())
                self.assertFalse(
                    (self.root / f"w{code}" / "home" / ".blaze" / "v2" / "build-script").exists()
                )

    def test_without_a_builder_no_module_is_asked_for(self) -> None:
        requests, environments, builder = self.fake_builder({})
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
            ("bogus", 1, "not a field of Input"),
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
