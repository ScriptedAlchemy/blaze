#!/usr/bin/env python3
"""Run the install workflow's actual channel shell against an isolated release service.

Network, installer and installed-binary commands are test doubles. This exercises the
workflow oracle and all existing channel/pin calls, not a real Blaze installation.
"""
import json
import os
import shlex
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

WORKFLOW = Path(__file__).resolve().parents[1] / 'workflows' / 'install.yaml'


def channels_step(path):
    lines = path.read_text().splitlines()
    starts = [i for i, line in enumerate(lines) if line == '      - name: channels']
    if len(starts) != 1:
        raise AssertionError('install workflow must have exactly one channels step')
    begin = starts[0] + 1
    while begin < len(lines) and lines[begin] != '        run: |':
        if lines[begin].startswith('      - '):
            raise AssertionError('channels step has no shell body')
        begin += 1
    if begin == len(lines):
        raise AssertionError('channels step has no shell body')
    body = []
    for line in lines[begin+1:]:
        if line and not line.startswith('          '):
            break
        body.append(line[10:] if line else '')
    return '\n'.join(body) + '\n'


DOUBLE = r'''
import json, os, pathlib, sys
home = pathlib.Path(os.environ['TEST_HOME'])
config = json.loads((home/'config.json').read_text())
name = pathlib.Path(sys.argv[0]).stem
args = sys.argv[1:]
with (home/'calls.jsonl').open('a') as log:
    log.write(json.dumps([name,args])+'\n')
if name == 'gh':
    print('the release service is not the repository GitHub releases', file=sys.stderr)
    sys.exit(99)
if name == 'curl':
    url = args[-1]
    origin = config['origin'].rstrip('/')
    if config.get('unavailable'):
        sys.exit(22)
    if url not in [origin+'/channels/stable', origin+'/channels/canary']:
        print('wrong release URL: '+url, file=sys.stderr)
        sys.exit(23)
    key = url.rsplit('/',1)[1]
    sys.stdout.write(config[key])
elif name == 'sh':
    if not args or args[0] != 'site/install.sh':
        sys.exit(98)
    channel = os.environ.get('BLAZE_CHANNEL', 'stable')
    version = os.environ.get('BLAZE_VERSION', '')
    it = iter(args[1:])
    for arg in it:
        if arg == '--channel': channel = next(it)
        elif arg == '--version': version = next(it)
        elif arg != '--no-toolchain': sys.exit(97)
    if version in ('stable','canary'):
        channel,version = version,''
    if channel not in ('stable','canary'):
        print('error: blaze has no channel '+channel+'.')
        sys.exit(1)
    pinned = bool(version)
    version = version.removeprefix('v') if pinned else config[channel].strip()
    (home/'installed').write_text(version)
    suffix = ' (canary)' if channel == 'canary' and not pinned else ''
    print('blaze '+version+suffix+' is installed.')
elif name == 'blaze':
    if args != ['--version']: sys.exit(96)
    print('blaze '+(home/'installed').read_text()+' test-binary')
'''


class ChannelTests(unittest.TestCase):
    def exercise(self, *, origin='https://blaze-releases.ix.dev', stable='0.20261027.0\n',
                 canary='0.20261105.4\n', unavailable=False):
        with tempfile.TemporaryDirectory() as directory:
            home=Path(directory)
            tools=home/'bin'; tools.mkdir()
            for name in ['curl','gh','sh','blaze']:
                implementation=tools/(name+'.py')
                implementation.write_text(textwrap.dedent(DOUBLE))
                script=tools/name
                script.write_text('#!/bin/sh\nexec '+shlex.quote(sys.executable)+
                                  ' -I -S '+shlex.quote(str(implementation))+' "$@"\n')
                script.chmod(0o700)
            (home/'config.json').write_text(json.dumps(dict(origin=origin,stable=stable,canary=canary,unavailable=unavailable)))
            env={k:v for k,v in os.environ.items() if not k.startswith('BLAZE_')}
            env.update(TEST_HOME=str(home),CARGO_HOME=str(home),RUNNER_TEMP=str(home),
                       PATH=str(tools)+os.pathsep+os.environ['PATH'],
                       BLAZE_REPO='ScriptedAlchemy/blaze',BLAZE_RELEASES=origin,
                       BLAZE_VERSION='a-dispatch-pin-must-not-affect-channel-checks')
            result=subprocess.run(['bash','-e','-o','pipefail','-c',channels_step(WORKFLOW)],
                                  cwd=home,env=env,text=True,capture_output=True,timeout=30)
            calls=[json.loads(line) for line in (home/'calls.jsonl').read_text().splitlines()] if (home/'calls.jsonl').exists() else []
            return result,calls

    def test_fork_without_github_releases_keeps_every_channel_and_pin_check(self):
        result,calls=self.exercise()
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)
        self.assertEqual([args[-1] for name,args in calls if name=='curl'],
                         ['https://blaze-releases.ix.dev/channels/stable','https://blaze-releases.ix.dev/channels/canary'])
        self.assertFalse(any(name=='gh' for name,_ in calls))
        installs=[args for name,args in calls if name=='sh']
        self.assertEqual(len(installs),8) # seven positive pin/channel variants plus rejection
        self.assertEqual(installs[-1],['site/install.sh','--channel','nightly'])
        self.assertEqual(sum(name=='blaze' for name,_ in calls),7)

    def test_custom_https_origin_is_used_consistently(self):
        result,calls=self.exercise(origin='https://release.example.test/blaze/')
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)
        self.assertEqual([args[-1] for name,args in calls if name=='curl'],
                         ['https://release.example.test/blaze/channels/stable','https://release.example.test/blaze/channels/canary'])

    def test_bad_pointer_cannot_become_a_guessed_release(self):
        for value in ['', 'not-a-version\n', 'v0.20261105.4\n', '0.1.2\n0.1.3\n']:
            with self.subTest(value=value):
                result,calls=self.exercise(stable=value)
                self.assertNotEqual(result.returncode,0)
                self.assertFalse(any(name in ('sh','blaze') for name,_ in calls))

    def test_fetch_failure_stops_before_installing(self):
        result,calls=self.exercise(unavailable=True)
        self.assertNotEqual(result.returncode,0)
        self.assertFalse(any(name in ('sh','blaze') for name,_ in calls))

    def test_insecure_origin_is_refused_before_network(self):
        result,calls=self.exercise(origin='http://release.example.test')
        self.assertNotEqual(result.returncode,0)
        self.assertEqual(calls,[])


if __name__=='__main__':
    unittest.main()
