"""Runtime checks must not disappear for dependency edits or renamed files."""
import json
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / '.github/scripts/changes.py'


class ChangeSelectionTests(unittest.TestCase):
    def select(self, files, compare=False):
        text = json.dumps(files) if compare else '\n'.join(files)
        result = subprocess.run([sys.executable, '-I', str(SCRIPT), *(['--compare-json'] if compare else [])],
                                input=text, text=True, capture_output=True, cwd=ROOT, check=True)
        return dict(line.split('=', 1) for line in result.stdout.splitlines())

    def test_runtime_dependencies_select_both_runtime_consumers(self):
        for path in ('crates/blaze-build-script/src/env.rs', 'crates/ra-jit-expand/src/lib.rs',
                     'vendor/rust-analyzer/crates/hir-ty/src/mir/eval.rs',
                     'vendor/rustc_abi/src/lib.rs', 'crates/blaze-registry/src/lib.rs'):
            with self.subTest(path=path):
                hit = self.select([path])
                self.assertEqual((hit['dist'], hit['serve']), ('true', 'true'))

    def test_rename_checks_the_old_path_as_well_as_destination(self):
        hit = self.select({'files': [{'filename': 'docs/removed.md',
                                     'previous_filename': 'crates/ra-jit-expand/src/lib.rs'}]}, True)
        self.assertEqual((hit['dist'], hit['serve']), ('true', 'true'))

    def test_truncated_or_malformed_compare_fails_closed(self):
        for data in ({}, {'files': [{'filename': 'README.md'}] * 300},
                     {'files': [{'status': 'changed'}]}, {'files': 'not a file list'}):
            with self.subTest(data=str(data)[:80]):
                hit = self.select(data, True)
                self.assertTrue(all(hit[area] == 'true' for area in ('dist', 'serve', 'bench', 'nix')))

    def test_documentation_only_remains_selective(self):
        for compare in (False, True):
            data = {'files': [{'filename': 'README.md'}]} if compare else ['README.md']
            hit = self.select(data, compare)
            self.assertEqual(hit['code'], 'false')

    def test_bench_only_does_not_run_the_compiler_runtime_matrix(self):
        hit = self.select(['crates/ra-jit-bench/benches/store.rs'])
        self.assertEqual((hit['bench'], hit['dist'], hit['serve']), ('true', 'false', 'false'))

    def test_invalid_json_runs_every_area_instead_of_skipping(self):
        result = subprocess.run([sys.executable, '-I', str(SCRIPT), '--compare-json'],
                                input='broken response', text=True, capture_output=True, cwd=ROOT)
        self.assertEqual(result.returncode, 0)
        self.assertIn('dist=true', result.stdout)
        self.assertIn('serve=true', result.stdout)


if __name__ == '__main__':
    unittest.main()
