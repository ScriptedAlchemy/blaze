#!/usr/bin/env python3
"""Check nocompact's Mach-O symbol lookup without requiring macOS or an ra-jit build."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from nocompact import symbol_address


class Symbols(unittest.TestCase):
    def test_legacy_and_current_private_names(self):
        for name in ("_ra-jit::answer::12", "_ra_jit::answer::12",
                     "_ra-jit::answer::n12", "_ra_jit::answer::n12"):
            with self.subTest(name=name):
                self.assertEqual(symbol_address(f"0000f0af t {name}\n", "answer"), "0x0000f0af")

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
