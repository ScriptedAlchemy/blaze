#!/usr/bin/env python3
"""Exercise the real, separately compiled ra-jit build script on source trees."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = os.environ.get("BLAZE_BUILD_SCRIPT")


@unittest.skipUnless(SCRIPT, "set BLAZE_BUILD_SCRIPT to the compiled ra-jit build.rs")
class SourceIdentityTests(unittest.TestCase):
    def setUp(self):
        self.work = tempfile.TemporaryDirectory()
        self.addCleanup(self.work.cleanup)
        self.root = Path(self.work.name) / "source"
        for name, contents in {
            "Cargo.toml": "[workspace]\n",
            "Cargo.lock": "version = 4\n",
            ".cargo/config.toml": "[build]\n",
            "harness/toolchain": "1.99.0\n",
            "crates/example/src/lib.rs": "pub const VALUE: u8 = 1;\n",
            "vendor/example/src/lib.rs": "pub fn vendored() {}\n",
        }.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)

    def identity(self, root=None, **extra):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("CARGO_FEATURE_", "CARGO_CFG_"))
               and k not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
        env.update(CARGO_WORKSPACE_DIR=str(root or self.root), TARGET="test-target")
        env.update(extra)
        result = subprocess.run([SCRIPT], env=env, capture_output=True, text=True,
                                errors="replace", timeout=10, check=True)
        identities = [line.split("=", 2)[2] for line in result.stdout.splitlines()
                      if line.startswith("cargo:rustc-env=RA_JIT_SOURCES=")]
        self.assertLessEqual(len(identities), 1)
        if identities:
            self.assertRegex(identities[0], r"^[0-9a-f]{64}$")
        return identities[0] if identities else None

    def test_relocated_sources_keep_the_same_identity(self):
        elsewhere = self.root.parent / "elsewhere"
        shutil.copytree(self.root, elsewhere)
        self.assertIsNotNone(self.identity())
        self.assertEqual(self.identity(), self.identity(elsewhere))

    def test_regular_source_edits_change_identity(self):
        before = self.identity()
        (self.root / "crates/example/src/lib.rs").write_text("pub const VALUE: u8 = 2;\n")
        self.assertNotEqual(before, self.identity())

    def test_compilation_features_and_target_are_part_of_identity(self):
        before = self.identity()
        self.assertNotEqual(before, self.identity(CARGO_FEATURE_LLVM="1"))
        self.assertNotEqual(before, self.identity(TARGET="another-target"))
        self.assertNotEqual(before, self.identity(CARGO_CFG_DEBUG_ASSERTIONS=""))

    def test_target_features_select_distinct_compiler_identities(self):
        first = self.identity(CARGO_CFG_TARGET_FEATURE="sse,sse2")
        second = self.identity(CARGO_CFG_TARGET_FEATURE="sse,sse2,avx2")
        self.assertIsNotNone(first)
        self.assertNotEqual(first, second)

    def test_custom_cfg_values_select_distinct_compiler_identities(self):
        first = self.identity(CARGO_CFG_BLAZE_BUILD_MODE="owned")
        second = self.identity(CARGO_CFG_BLAZE_BUILD_MODE="mixed")
        self.assertIsNotNone(first)
        self.assertNotEqual(first, second)

    def test_custom_cfg_presence_is_not_an_unconfigured_compiler(self):
        self.assertNotEqual(self.identity(), self.identity(CARGO_CFG_BLAZE_SPECIAL=""))

    def test_encoded_cfg_argument_boundaries_are_not_collapsed(self):
        # Cargo's comma-joined cfg value cannot distinguish one value containing
        # a comma from two separate cfg values. Keep the original flag boundaries.
        first = self.identity(CARGO_CFG_BLAZE_MODE="a,b",
                              CARGO_ENCODED_RUSTFLAGS='--cfg\x1fblaze_mode="a,b"')
        second = self.identity(CARGO_CFG_BLAZE_MODE="a,b",
                               CARGO_ENCODED_RUSTFLAGS='--cfg\x1fblaze_mode="a"\x1f--cfg\x1fblaze_mode="b"')
        self.assertNotEqual(first, second)

    def test_plain_rustflags_are_kept_when_encoded_flags_are_absent(self):
        self.assertNotEqual(self.identity(), self.identity(RUSTFLAGS="--cfg=blaze_special"))

    def test_encoded_flags_override_plain_flags_including_an_empty_value(self):
        first = self.identity(CARGO_ENCODED_RUSTFLAGS="", RUSTFLAGS="--cfg=ignored_a")
        second = self.identity(CARGO_ENCODED_RUSTFLAGS="", RUSTFLAGS="--cfg=ignored_b")
        self.assertEqual(first, second)

    @unittest.skipUnless(os.name == "posix", "Unix native Rust flags fixture")
    def test_non_unicode_effective_rustflags_use_binary_identity(self):
        self.assertIsNone(self.identity(CARGO_ENCODED_RUSTFLAGS=os.fsdecode(b"\xff")))

    def test_cfg_insertion_order_does_not_change_identity(self):
        first = self.identity(CARGO_CFG_ALPHA="1", CARGO_CFG_BETA="2")
        second = self.identity(CARGO_CFG_BETA="2", CARGO_CFG_ALPHA="1")
        self.assertIsNotNone(first)
        self.assertEqual(first, second)

    @unittest.skipUnless(os.name == "posix", "Unix native cfg value fixture")
    def test_non_unicode_cfg_values_use_binary_identity(self):
        self.assertIsNone(self.identity(CARGO_CFG_BLAZE_SPECIAL=os.fsdecode(b"\xff")))

    @unittest.skipUnless(os.name == "posix", "Unix native cfg name fixture")
    def test_non_unicode_cfg_names_use_binary_identity(self):
        self.assertIsNone(self.identity(**{os.fsdecode(b"CARGO_CFG_\xff"): "1"}))

    @unittest.skipUnless(os.name == "posix", "Unix native environment fixture")
    def test_unrelated_non_unicode_environment_is_not_decoded(self):
        before = self.identity()
        self.assertEqual(before, self.identity(UNRELATED_NATIVE_VALUE=os.fsdecode(b"\xff")))

    def test_git_metadata_file_is_not_compiler_source(self):
        before = self.identity()
        (self.root / "vendor/example/.git").write_text("gitdir: /machine/one\n")
        self.assertEqual(before, self.identity())
        (self.root / "vendor/example/.git").write_text("gitdir: /machine/two\n")
        self.assertEqual(before, self.identity())

    @unittest.skipUnless(os.name == "posix", "Unix source alias fixture")
    def test_file_symlink_referent_contents_are_hashed(self):
        external = self.root.parent / "external.rs"
        external.write_text("1")
        (self.root / "vendor/alias.rs").symlink_to(external)
        before = self.identity()
        external.write_text("2")
        self.assertNotEqual(before, self.identity())

    def test_generated_output_directories_do_not_invalidate_identity(self):
        before = self.identity()
        for directory in ("target", "node_modules", ".git"):
            path = self.root / "crates/example" / directory / "generated"
            path.parent.mkdir(parents=True)
            path.write_text("not a compiler source")
        self.assertEqual(before, self.identity())

    def test_hidden_source_directories_are_not_silently_omitted(self):
        path = self.root / "crates/example/.included/body.rs"
        path.parent.mkdir()
        path.write_text("1")
        before = self.identity()
        path.write_text("2")
        self.assertNotEqual(before, self.identity())

    @unittest.skipUnless(os.name == "posix", "Unix source alias fixture")
    def test_directory_symlink_referents_are_hashed(self):
        external = self.root.parent / "external"
        external.mkdir()
        (external / "lib.rs").write_text("1")
        (self.root / "vendor/alias").symlink_to(external, target_is_directory=True)
        before = self.identity()
        self.assertIsNotNone(before)
        (external / "lib.rs").write_text("2")
        self.assertNotEqual(before, self.identity())

    @unittest.skipUnless(os.name == "posix", "Unix source alias fixture")
    def test_source_alias_cycles_use_binary_identity(self):
        (self.root / "crates/example/cycle").symlink_to(self.root / "crates")
        self.assertIsNone(self.identity())

    @unittest.skipUnless(os.name == "posix", "Unix source alias fixture")
    def test_broken_source_alias_uses_binary_identity(self):
        (self.root / "vendor/broken.rs").symlink_to("missing-source.rs")
        self.assertIsNone(self.identity())

    @unittest.skipUnless(os.name == "posix", "Unix source path fixture")
    def test_non_unicode_source_paths_are_not_lossily_collapsed(self):
        path = os.fsencode(self.root / "vendor") + b"/bad-\xff.rs"
        with open(path, "wb") as out:
            out.write(b"fn source() {}")
        self.assertIsNone(self.identity())

    @unittest.skipUnless(os.name == "posix", "backslash is a Unix filename character")
    def test_backslash_filename_is_not_a_directory_separator(self):
        path = self.root / "vendor/a\\b.rs"
        path.write_text("fn source() {}")
        before = self.identity()
        path.unlink()
        path = self.root / "vendor/a/b.rs"
        path.parent.mkdir()
        path.write_text("fn source() {}")
        self.assertNotEqual(before, self.identity())

    @unittest.skipUnless(os.name == "posix" and hasattr(os, "geteuid") and os.geteuid() != 0,
                         "requires an unprivileged Unix reader")
    def test_unreadable_source_is_not_ignored(self):
        path = self.root / "crates/example/src/lib.rs"
        path.chmod(0)
        self.addCleanup(path.chmod, 0o600)
        self.assertIsNone(self.identity())

    @unittest.skipUnless(os.name == "posix" and hasattr(os, "geteuid") and os.geteuid() != 0,
                         "requires an unprivileged Unix reader")
    def test_unreadable_source_directory_is_not_ignored(self):
        path = self.root / "crates/example/src"
        path.chmod(0)
        self.addCleanup(path.chmod, 0o700)
        self.assertIsNone(self.identity())

    def test_missing_required_source_fact_uses_binary_identity(self):
        (self.root / "harness/toolchain").unlink()
        self.assertIsNone(self.identity())


if __name__ == "__main__":
    if not SCRIPT:
        raise SystemExit("BLAZE_BUILD_SCRIPT must name the compiled ra-jit build.rs")
    unittest.main()
