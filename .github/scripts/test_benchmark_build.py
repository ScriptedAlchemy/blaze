"""Benchmark alignment belongs to the producer, with identical recorded pair inputs."""
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bundle

ALIGN = ['-C', 'llvm-args=-align-all-functions=6',
         '-C', 'llvm-args=-align-all-nofallthru-blocks=5']


class BenchmarkBuildTest(unittest.TestCase):
    def build(self, environ):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            workspace, temp = root/'workspace', root/'temp'
            workspace.mkdir()
            (temp/'base-tree').mkdir(parents=True)
            specs = {
                'bench-head': {'platforms': ['linux-x64'], 'bench': ['--bench', 'store']},
                'bench-base': {'platforms': ['linux-x64'], 'source': 'base', 'bench': ['--bench', 'store']},
                'ordinary': {'platforms': ['linux-x64'], 'build': ['--bin', 'app']},
            }
            path = root/'sets.json'
            output = root/'out.json'
            path.write_text(json.dumps(specs))
            calls = []
            def cargo(command, cwd, env):
                calls.append((command, cwd, dict(env)))
                return []
            with mock.patch.dict(os.environ, environ, clear=True), \
                 mock.patch.object(bundle, 'roots', return_value={'workspace': str(workspace), 'temp': str(temp)}), \
                 mock.patch.object(bundle, 'host_triple', return_value='x86_64-unknown-linux-gnu'), \
                 mock.patch.object(bundle, 'source_identity', return_value={'commit': 'base', 'tree': 'tree'}), \
                 mock.patch.object(bundle, 'cargo_json', side_effect=cargo):
                bundle.build(SimpleNamespace(platform='linux-x64', sets=str(path), out=str(output), base_sha='base'))
            return json.loads(output.read_text()), calls

    def test_both_benchmarks_receive_alignment_and_record_it(self):
        records, calls = self.build({})
        for name, (_, _, env) in zip(('bench-head', 'bench-base'), calls[:2]):
            self.assertEqual(env.get('CARGO_ENCODED_RUSTFLAGS'), '\x1f'.join(ALIGN))
            self.assertEqual(records[name].get('rustflags'), ALIGN)
        self.assertNotIn('CARGO_ENCODED_RUSTFLAGS', calls[2][2])
        self.assertNotIn('rustflags', records['ordinary'])

    def test_inherited_flags_remain_identical_without_changing_other_builds(self):
        original = {'RUSTFLAGS': '-C debuginfo=1 --cfg bench_probe'}
        records, calls = self.build(original)
        expected = original['RUSTFLAGS'].split() + ALIGN
        self.assertEqual(records['bench-head'].get('rustflags'), expected)
        self.assertEqual(records['bench-base'].get('rustflags'), expected)
        self.assertNotIn('RUSTFLAGS', calls[0][2])
        self.assertEqual(calls[2][2]['RUSTFLAGS'], original['RUSTFLAGS'])
        self.assertNotIn('CARGO_ENCODED_RUSTFLAGS', calls[2][2])

    def test_encoded_flags_take_precedence_and_keep_argument_boundaries(self):
        original = {'CARGO_ENCODED_RUSTFLAGS': '--remap-path-prefix\x1f/a b=/c', 'RUSTFLAGS': '--cfg wrong'}
        records, calls = self.build(original)
        expected = ['--remap-path-prefix', '/a b=/c'] + ALIGN
        self.assertEqual(records['bench-head'].get('rustflags'), expected)
        self.assertEqual(calls[0][2]['CARGO_ENCODED_RUSTFLAGS'].split('\x1f'), expected)
        self.assertEqual(calls[2][2]['RUSTFLAGS'], '--cfg wrong')

    def test_empty_encoded_flags_override_plain_flags(self):
        records, _ = self.build({'CARGO_ENCODED_RUSTFLAGS': '', 'RUSTFLAGS': '--cfg wrong'})
        self.assertEqual(records['bench-head'].get('rustflags'), ALIGN)


if __name__ == '__main__':
    unittest.main()
