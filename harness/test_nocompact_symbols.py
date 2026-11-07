#!/usr/bin/env python3
"""Check exported Mach-O symbol lookup without needing macOS or a Rust build."""
import sys
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path
from subprocess import CompletedProcess
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
from nocompact import FILES, address_of, expanded_source, symbol_address


class Symbols(unittest.TestCase):
    def test_fixture_exports_the_edited_root(self):
        source = FILES["src/main.rs"]
        self.assertIn('#[inline(never)]\n#[unsafe(export_name = "ncu_answer")]\nfn answer()', source)
        self.assertIn("let call = std::hint::black_box(answer as fn() -> u32);", source)

    def test_growth_preserves_exported_noninline_answer(self):
        expanded = expanded_source(FILES["src/main.rs"], 3)
        self.assertEqual(expanded.count(
            '#[inline(never)]\n#[unsafe(export_name = "ncu_answer")]\nfn answer()'
        ), 1)
        self.assertIn("#[inline(never)]\nfn f0() -> u32", expanded)
        self.assertIn("fn f2() -> u32", expanded)
        self.assertIn("std::hint::black_box(f2 as fn() -> u32);", expanded)
        self.assertIn("let call = std::hint::black_box(answer as fn() -> u32);", expanded)
        with self.assertRaisesRegex(ValueError, "no unique no-inline answer"):
            expanded_source("fn answer() -> u32 {\n    1\n}\n", 3)

    def test_growth_adds_frames_not_only_trivial_leaf_symbols(self):
        # An address-taken constant-returning function can lack an unwind FDE
        # on arm64, making the original 1000-function stress test vacuous.
        expanded = expanded_source(FILES["src/main.rs"], 3)
        for i in range(3):
            with self.subTest(function=i):
                code = (f"#[inline(never)]\nfn f{i}() -> u32 {{\n"
                        "    let call = std::hint::black_box(answer as fn() -> u32);\n"
                        f"    call().wrapping_add({i})\n"
                        "}\n")
                self.assertIn(code, expanded)
                self.assertIn(f"std::hint::black_box(f{i} as fn() -> u32);", expanded)
        self.assertEqual(expanded.count('export_name = "ncu_answer"'), 1)

    def test_exported_macho_symbol_variants(self):
        for kind in "TtSs":
            for separator in (" ", "\t"):
                with self.subTest(kind=kind, separator=repr(separator)):
                    line = f"00aBcd09{separator}{kind}{separator}_ncu_answer\n"
                    self.assertEqual(symbol_address(line, "ncu_answer"), "0x00aBcd09")

    def test_no_private_or_unrelated_function_is_substituted(self):
        for name in ("_ra-jit::answer::12", "_ra_jit::ncu::answer::n12",
                     "_ncu_answer_extra", "__ncu_answer", "_ncu_answer::9", "_other"):
            with self.subTest(symbol=name):
                self.assertIsNone(symbol_address(f"0000f0af t {name}\n", "ncu_answer"))

    def test_no_non_code_or_malformed_symbol_is_used(self):
        for line in ("0000f0af d _ncu_answer\n",
                     "not-an-address t _ncu_answer\n", "0000f0af t _ncu_answer junk\n"):
            with self.subTest(line=line):
                self.assertIsNone(symbol_address(line, "ncu_answer"))

    def test_ambiguous_export_is_rejected(self):
        listed = "0000f0af t _ncu_answer\n0000f0ff T _ncu_answer\n"
        self.assertIsNone(symbol_address(listed, "ncu_answer"))

    def test_nm_failure_is_not_reported_as_absent_symbol(self):
        failed = CompletedProcess(["nm", "program"], 1, "", "malformed object")
        with mock.patch("nocompact.subprocess.run", return_value=failed):
            with self.assertRaisesRegex(SystemExit, "malformed object"):
                address_of("program", "ncu_answer")

    def test_missing_export_prints_relevant_nm_names(self):
        listing = "0000f0af T _ncu_answer_extra\n0000f0ff t _ra_jit::other::n7\n"
        completed = CompletedProcess(["nm", "program"], 0, listing, "")
        diagnostic = StringIO()
        with mock.patch("nocompact.subprocess.run", return_value=completed):
            with redirect_stderr(diagnostic):
                self.assertIsNone(address_of("program", "ncu_answer"))
        self.assertIn("_ncu_answer_extra", diagnostic.getvalue())
        self.assertIn("2 nm rows", diagnostic.getvalue())


if __name__ == "__main__":
    unittest.main()
