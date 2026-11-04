#!/usr/bin/env python3
"""Check nocompact's Mach-O symbol lookup without requiring macOS or an ra-jit build."""
import sys
import unittest
from contextlib import redirect_stderr
from io import StringIO
from subprocess import CompletedProcess
from unittest import mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from nocompact import FILES, address_of, expanded_source, symbol_address


class Symbols(unittest.TestCase):
    def test_growth_preserves_address_taken_noninline_answer(self):
        expanded = expanded_source(FILES["src/main.rs"], 3)
        self.assertEqual(expanded.count("#[inline(never)]\nfn answer() -> u32 {\n"), 1)
        self.assertNotIn("#[inline(never)]\nfn f0", expanded)
        self.assertIn("fn f2() -> u32", expanded)
        self.assertIn("std::hint::black_box(f2 as fn() -> u32);", expanded)
        self.assertIn("let call = std::hint::black_box(answer as fn() -> u32);", expanded)
        with self.assertRaisesRegex(ValueError, "no unique no-inline answer"):
            expanded_source("fn answer() -> u32 {\n    1\n}\n", 3)

    def test_legacy_and_current_private_names(self):
        for name in ("_ra-jit::answer::12", "_ra_jit::answer::12",
                     "_ra-jit::answer::n12", "_ra_jit::answer::n12"):
            with self.subTest(name=name):
                self.assertEqual(symbol_address(f"0000f0af t {name}\n", "answer"), "0x0000f0af")

    def test_qualified_private_names_keep_the_exact_final_function(self):
        for symbol in ("_ra_jit::ncu::answer::n7",
                       "_ra-jit::app::nested::answer::42"):
            with self.subTest(symbol=symbol):
                self.assertEqual(symbol_address(f"00aBcd09 t {symbol}\n", "answer"),
                                 "0x00aBcd09")
        self.assertIsNone(symbol_address(
            "0000f0af t _ra_jit::app::answer_extra::n7\n", "answer"))
        # Two matching functions in different modules are ambiguous, not a
        # reason to guess the first function's unwind address.
        self.assertIsNone(symbol_address(
            "0000f0af t _ra_jit::app::answer::n7\n"
            "0000f0ff t _ra_jit::other::answer::n8\n", "answer"))

    def test_global_text_and_tab_separated_nm_symbols(self):
        for kind in ("T", "S", "t", "s"):
            with self.subTest(kind=kind):
                self.assertEqual(symbol_address(f"00aBcd09\t{kind}\t_ra_jit::answer::n7\n",
                                                "answer"), "0x00aBcd09")

    def test_nm_error_is_not_reported_as_missing_symbol(self):
        failed = CompletedProcess(["nm", "program"], 1, "", "malformed object")
        with mock.patch("nocompact.subprocess.run", return_value=failed):
            with self.assertRaisesRegex(SystemExit, "malformed object"):
                address_of("program", "answer")

    def test_missing_symbol_reports_actual_nm_candidates(self):
        listing = "0000f0af T _ra_jit::unexpected::n7\n"
        completed = CompletedProcess(["nm", "program"], 0, listing, "")
        diagnostic = StringIO()
        with mock.patch("nocompact.subprocess.run", return_value=completed):
            with redirect_stderr(diagnostic):
                self.assertIsNone(address_of("program", "answer"))
        self.assertIn("_ra_jit::unexpected::n7", diagnostic.getvalue())
        self.assertIn("1 nm rows", diagnostic.getvalue())

    def test_does_not_use_a_different_function(self):
        self.assertIsNone(symbol_address("0000f0af t _ra_jit::another::n12\n", "answer"))

    def test_rejects_non_code_and_malformed_symbols(self):
        for line in ("0000f0af d _ra_jit::answer::n12\n",
                     "0000f0af t _ra_jit::answer::not-a-number\n",
                     "not-an-address t _ra_jit::answer::12\n"):
            with self.subTest(line=line):
                self.assertIsNone(symbol_address(line, "answer"))

    def test_ambiguous_function_is_not_guessed(self):
        listed = ("0000f0af t _ra_jit::answer::n12\n"
                  "0000f0ff t _ra_jit::answer::n13\n")
        self.assertIsNone(symbol_address(listed, "answer"))


if __name__ == "__main__":
    unittest.main()
