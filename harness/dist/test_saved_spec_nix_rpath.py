"""The no-RPATH control must work with target-salted Nix wrappers."""
import unittest
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from saved_spec import without_nix_rpath


class NixRunPathTests(unittest.TestCase):
    def test_salted_wrapper_switch_and_explicit_rpath_removal(self):
        env = {
            "NIX_BINTOOLS_WRAPPER_TARGET_HOST_x86_64_unknown_linux_gnu": "1",
            "NIX_LDFLAGS_x86_64_unknown_linux_gnu":
                "-L/nix/store/xxx/lib -rpath /home/runner/outputs/out/lib -lm",
            "NIX_LDFLAGS_BEFORE_x86_64_unknown_linux_gnu": "-Wl,-rpath,/temporary -z now",
        }
        result = without_nix_rpath(env)
        self.assertEqual(result["NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu"], "1")
        self.assertEqual(result["NIX_LDFLAGS_x86_64_unknown_linux_gnu"],
                         "-L/nix/store/xxx/lib -lm")
        self.assertEqual(result["NIX_LDFLAGS_BEFORE_x86_64_unknown_linux_gnu"], "-z now")
        self.assertEqual(env["NIX_LDFLAGS_x86_64_unknown_linux_gnu"].count("-rpath"), 1)

    def test_cross_target_discovered_from_wrapper_not_host(self):
        result = without_nix_rpath({
            "NIX_CC_WRAPPER_TARGET_HOST_aarch64_unknown_linux_gnu": "1",
            "NIX_DONT_SET_RPATH": "1",
        })
        self.assertEqual(result["NIX_DONT_SET_RPATH_aarch64_unknown_linux_gnu"], "1")
        self.assertNotIn("NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu", result)

    def test_nix_flags_preserve_non_rpath_library_search(self):
        env = {"NIX_LDFLAGS": "-L/opt/lib --rpath=/tmp/bad -lfoo -rpath /tmp/other"}
        self.assertEqual(without_nix_rpath(env)["NIX_LDFLAGS"], "-L/opt/lib -lfoo")


if __name__ == "__main__":
    unittest.main()
