#!/usr/bin/env python3
"""Install channel and pinned release fixtures with the shipped installer and an offline curl."""

import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[2] / 'site/install.sh'
STABLE = '0.20261027.0'
CANARY = '0.20261105.4'
ORIGIN = 'https://blaze-releases.ix.dev'


class ChannelTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.service = self.root / 'releases'
        tools = self.root / 'tools'
        tools.mkdir()
        channels = self.service / 'channels'
        channels.mkdir(parents=True)
        for channel, version in [('stable', STABLE), ('canary', CANARY)]:
            (channels / channel).write_text(version + '\n')
            release = self.service / ('v' + version)
            release.mkdir()
            program = self.root / 'blaze'
            program.write_text(f'#!/bin/sh\necho "blaze {version}"\n')
            program.chmod(0o755)
            asset = release / f'blaze-v{version}-aarch64-apple-darwin.tar.gz'
            with tarfile.open(asset, 'w:gz') as tar:
                tar.add(program, 'release/blaze')
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            (release / 'SHA256SUMS').write_text(f'{digest}  {asset.name}\n')
        # Fix the target so the same archive exercises the installer on every CI host.
        for name, body in {
            'uname': 'case "$1" in -s) echo Darwin;; -m) echo arm64;; *) exit 2;; esac',
            'curl': '''while [ "$1" != -o ]; do shift; done
out=$2; url=$3
printf '%s\\n' "$url" >> "$HOME/requests"
case "$url" in "$TEST_ORIGIN"/*) cp "$HOME/releases/${url#"$TEST_ORIGIN"/}" "$out";; *) exit 22;; esac''',
        }.items():
            program = tools / name
            program.write_text('#!/bin/sh\nset -eu\n' + body + '\n')
            program.chmod(0o755)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith('BLAZE_')}
        self.env.update(HOME=str(self.root), CARGO_HOME=str(self.root / '.cargo'),
                        PATH=str(tools) + os.pathsep + self.env['PATH'])

    def install(self, *args, origin=ORIGIN, **settings):
        self.requests = self.root / 'requests'
        self.requests.write_text('')
        return subprocess.run(['sh', str(INSTALLER), '--no-toolchain', *args],
                              env=dict(self.env, BLAZE_RELEASES=origin, TEST_ORIGIN=origin.rstrip('/'), **settings),
                              capture_output=True, text=True, timeout=15)

    def assert_installs(self, args, settings, version, channel=None, origin=ORIGIN):
        result = self.install(*args, origin=origin, **settings)
        self.assertEqual(result.returncode, 0, result.stderr)
        label = ' (canary)' if channel == 'canary' else ''
        self.assertIn(f'blaze {version}{label} is installed.', result.stdout)
        program = self.root / '.cargo/bin/blaze'
        self.assertEqual(subprocess.check_output([program, '--version'], text=True, timeout=15).strip(), f'blaze {version}')
        release = f'/v{version}'
        paths = ([f'/channels/{channel}'] if channel else []) + [
            f'{release}/SHA256SUMS', f'{release}/blaze-v{version}-aarch64-apple-darwin.tar.gz']
        self.assertEqual(self.requests.read_text().splitlines(), [origin.rstrip('/') + path for path in paths])

    def test_channels_and_pins(self):
        for args, env, version, channel in [
            ([], {}, STABLE, 'stable'), (['--channel', 'stable'], {}, STABLE, 'stable'),
            (['--version', 'stable'], {}, STABLE, 'stable'), (['--channel', 'canary'], {}, CANARY, 'canary'),
            ([], {'BLAZE_CHANNEL': 'canary'}, CANARY, 'canary'), (['--version', 'canary'], {}, CANARY, 'canary'),
            (['--channel', 'canary', '--version', 'v' + STABLE], {}, STABLE, None),
            ([], {'BLAZE_VERSION': CANARY}, CANARY, None),
        ]:
            with self.subTest(args=args, env=env):
                self.assert_installs(args, env, version, channel)

    def test_custom_https_origin_is_used_consistently(self):
        self.assert_installs([], {}, STABLE, 'stable', 'https://release.example.test/blaze/')

    def test_bad_pointer_stops_before_fetching_an_archive(self):
        for value in ['', 'not-a-version\n', 'v0.20261105.4\n', '0.1.2\n0.1.3\n']:
            with self.subTest(value=value):
                (self.service / 'channels/stable').write_text(value)
                self.assertNotEqual(self.install().returncode, 0)
                self.assertEqual(self.requests.read_text().splitlines(), [ORIGIN + '/channels/stable'])
                self.assertFalse((self.root / '.cargo/bin/blaze').exists())

    def test_fetch_failure_stops_before_installing(self):
        (self.service / 'channels/stable').unlink()
        self.assertNotEqual(self.install().returncode, 0)
        self.assertFalse((self.root / '.cargo/bin/blaze').exists())

    def test_unknown_channel_and_insecure_origin_are_refused_before_network(self):
        for args, origin, diagnostic in [(['--channel', 'nightly'], ORIGIN, 'no channel nightly'),
                                         ([], 'http://release.example.test', 'not an https:// address')]:
            with self.subTest(args=args, origin=origin):
                result = self.install(*args, origin=origin)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic, result.stderr)
                self.assertEqual(self.requests.read_text(), '')


if __name__ == '__main__':
    unittest.main()
