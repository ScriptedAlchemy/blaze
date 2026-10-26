"""Lightweight tests of the measurement driver; no daemon or container starts."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


def module(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


class FrontImageTests(unittest.TestCase):
    def test_parser_records_real_ready_and_image_counts(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            (root / "logs").mkdir()
            (root / "result.json").write_text(json.dumps({
                "outcome": "stopped",
                "stopped_after": {"footprint_mb": 100, "rss_mb": 140},
            }))
            (root / "logs" / "worker.log").write_text(
                "load workspace: 1.2s\nresolve: 42 crates in 30.0ms\n"
                "front-end serve: 10ms in all, 2ms of it reading parts, 3 of 3 parts from the image\n"
                "front-end parts: def maps 1.0 MB, impls 2.0 MB, lang items 0.1 MB\n"
                "front-end census: dependency, not frozen: 2 crates, 5 modules, 4 files, 0.3 MB\n"
                "front-end ready\n"
            )
            row = driver.parse(work)
            self.assertEqual(row["crates"], 42)
            self.assertEqual(row["from_image"], 3)
            self.assertEqual(row["resolve_s"], 0.03)
            self.assertEqual(row["ready_footprint_mb"], 100)
            self.assertEqual(row["census"]["dependency, not frozen"]["crates"], 2)

    def test_pressure_rejects_a_container_larger_than_the_guard(self):
        pressure = module("frontimage_pressure")
        import argparse
        with self.assertRaises(ValueError):
            pressure.host(argparse.Namespace(path=__file__, size_mb=6144, extra_mb=100,
                                             runs=3, share=False))

    def test_spread_is_explicit(self):
        driver = module("frontimage")
        self.assertEqual(driver.spread([None]), "-")
        self.assertEqual(driver.spread([1, 2, 3]), "2.00 (1.00-3.00)")


if __name__ == "__main__":
    unittest.main()
