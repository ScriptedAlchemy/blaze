#!/usr/bin/env python3
"""Read-only retained Actions-history census. No builds, reruns, or repository writes."""
import concurrent.futures as futures
import collections
import datetime as dt
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import sys
import threading
import time
import urllib.error
import urllib.request
import zipfile

REPO = os.environ['GITHUB_REPOSITORY']
API = 'https://api.github.com/repos/' + REPO
OUT = Path(os.environ.get('AUDIT_OUT', 'ci-history'))
OUT.mkdir(parents=True, exist_ok=True)
TOKEN = os.environ['GH_TOKEN']
LOCK = threading.Lock()
STATE = {'api_requests': 0, 'rate_remaining': None, 'rate_limit': None}

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None
OPENER = urllib.request.build_opener(NoRedirect())


def save(name, data):
    path = OUT / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=1, ensure_ascii=True))


def request(path, binary=False):
    url = API + path
    headers = {'Authorization': 'Bearer ' + TOKEN, 'Accept': 'application/vnd.github+json',
               'X-GitHub-Api-Version': '2022-11-28', 'User-Agent': 'blaze-ci-history-audit'}
    for attempt in range(3):
        with LOCK:
            if STATE['rate_remaining'] is not None and STATE['rate_remaining'] < 8:
                raise RuntimeError('API rate budget exhausted; item recorded as uninspected')
            STATE['api_requests'] += 1
        try:
            try:
                response = OPENER.open(urllib.request.Request(url, headers=headers), timeout=90)
            except urllib.error.HTTPError as e:
                response = e
            with response:
                with LOCK:
                    for key, header in [('rate_remaining', 'X-RateLimit-Remaining'),
                                        ('rate_limit', 'X-RateLimit-Limit')]:
                        if response.headers.get(header):
                            STATE[key] = int(response.headers[header])
                if response.code in (301, 302, 303, 307, 308) and binary:
                    # Authorization never travels to GitHub's signed storage URL.
                    redirect = response.headers['Location']
                    with urllib.request.urlopen(redirect, timeout=120) as remote:
                        return remote.read()
                if response.code >= 400:
                    message = response.read().decode('utf8', 'replace')[:500]
                    if response.code in (429, 500, 502, 503, 504) and attempt < 2:
                        time.sleep(2 ** (attempt + 1))
                        continue
                    raise RuntimeError('HTTP ' + str(response.code) + ': ' + message)
                data = response.read()
                return data if binary else json.loads(data)
        except (TimeoutError, OSError) as e:
            if attempt == 2:
                raise RuntimeError(type(e).__name__ + ': ' + str(e)) from e
            time.sleep(attempt + 1)


def pages(path, field):
    data = []
    page = 1
    sep = '&' if '?' in path else '?'
    expected = None
    while True:
        reply = request(path + sep + 'per_page=100&page=' + str(page))
        rows = reply if field is None else reply.get(field, [])
        if isinstance(reply, dict):
            expected = reply.get('total_count', expected)
        data.extend(rows)
        print('PAGE', path, page, 'rows', len(rows), 'total', len(data), 'expected', expected, flush=True)
        if len(rows) < 100:
            return data, {'pages': page, 'expected_count': expected, 'retrieved_count': len(data),
                          'terminated_on_short_page': True}
        page += 1


ANSI = re.compile(r'\x1b\[[0-9;]*[a-zA-Z]')
DATE = re.compile(r'^\ufeff?\d{4}-\d\d-\d\dT[^ ]+ ?')
SIGNAL = re.compile(r'(?:##\[error\]|test result: FAILED|\.\.\. FAILED\b|^failures:|^error(?:\[E[0-9]+\])?:|panicked at|assertion.*failed|Traceback \(most recent call last\)|AssertionError|^FAIL\b|No space left on device|undefined (?:symbol|reference)|SIG(?:SEGV|ABRT|PIPE|KILL)|segmentation fault|memory allocation of|out of memory|watchdog.*(?:trip|kill|exceed)|timed? ?out|timeout exceeded|Process completed with exit code [1-9]|[1-9][0-9]* functions in it ra-jit could not compile|native_root|PlatformMismatch|AddressSanitizer|ThreadSanitizer|data race|^error:.*(?:mismatch|failed))', re.I)
TEST = re.compile(r'test result: (?:ok|FAILED)\..*|(?:Ran [0-9]+ tests? in .*|[0-9]+ passed, [0-9]+ failed.*|all cases passed)$')
WARNING = re.compile(r'(?:##\[warning\]|test .* has been running for over|no tests (?:ran|were)|running 0 tests|uncompiled|refused|skip(?:ped|ping).*(?:test|check|gate)|cache.*(?:corrupt|mismatch)|forbidden.*(?:attempt|exec))', re.I)
SECRET = re.compile(r'(?i)((?:authorization|access_token|github_token|gh_token|password|secret)[=: ]+)[^\s,;]+')


def normalize(line):
    line = re.sub(r'/home/runner/work/_temp/[^ /:]+', '<temp>', line)
    line = re.sub(r'/nix/store/[a-z0-9]{32}-', '/nix/store/<hash>-', line)
    line = re.sub(r'/tmp/[^ /:]+', '<tmp>', line)
    line = re.sub(r'\b[0-9a-f]{24,64}\b', '<hash>', line)
    line = re.sub(r'\b[0-9]+\.[0-9]+(?:s|ms| MiB| MB)\b', '<measure>', line)
    return line.strip()


def scan_text(text, name):
    rawlines = text.splitlines()
    lines = [ANSI.sub('', DATE.sub('', line)) for line in rawlines]
    signatures = {}
    tests = []
    warnings = collections.Counter()
    for i, line in enumerate(lines):
        if TEST.search(line):
            tests.append({'line': i + 1, 'text': line})
        if WARNING.search(line):
            warnings[normalize(line)] += 1
        # Commands and source snippets printed by the runner are not themselves failures.
        if '\x1b[36;' in rawlines[i] or line.startswith('##[group]Run '):
            continue
        if SIGNAL.search(line):
            key = normalize(line)
            if key not in signatures:
                signatures[key] = {'signature': key, 'count': 0, 'line': i + 1,
                                   'context': '\n'.join(lines[max(0, i-4):min(len(lines), i+8)])}
            signatures[key]['count'] += 1
    return {'file': name, 'lines': len(lines), 'signals': list(signatures.values()),
            'tests': tests, 'warnings': dict(warnings), 'tail': '\n'.join(lines[-25:])}


def collect_archive(data, ident, kind):
    summaries = []
    texts = {}
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        members = [n for n in archive.namelist() if not n.endswith('/')]
        if kind == 'logs':
            whole = [n for n in members if '/' not in n and n.endswith('.txt')]
            selected = whole if whole else [n for n in members if n.endswith(('.txt', '.log'))]
        else:
            selected = [n for n in members if n.endswith(('.log', '.txt', '.json', '.jsonl', '.csv', '.md', '.stderr', '.stdout'))]
        for name in selected:
            raw = archive.read(name)
            try:
                text = raw.decode('utf8-sig')
            except UnicodeDecodeError:
                text = raw.decode('utf8', 'replace')
            text = SECRET.sub(r'\1<redacted>', text)
            texts[name] = text
            summaries.append(scan_text(text, name))
    path = OUT / kind / (str(ident) + '.json.gz')
    path.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(path, 'wt', encoding='utf8') as f:
        json.dump(texts, f, ensure_ascii=True)
    return {'archive_bytes': len(data), 'archive_sha256': hashlib.sha256(data).hexdigest(),
            'archive_members': len(members), 'text_members': len(texts), 'files': summaries}


def inventory():
    runs, run_coverage = pages('/actions/runs', 'workflow_runs')
    by_id = {r['id']: r for r in runs if r.get('head_branch') != 'audit/ci-history-20261031'}
    runs = list(by_id.values())
    save('runs.json', runs)
    artifacts, artifact_coverage = pages('/actions/artifacts', 'artifacts')
    save('artifacts.json', artifacts)
    prs, _ = pages('/pulls?state=all', None)
    save('pull_requests.json', prs)
    workflows, _ = pages('/actions/workflows', 'workflows')
    save('workflows.json', workflows)
    summary = {'snapshot_utc': dt.datetime.now(dt.timezone.utc).isoformat(), 'repository': REPO,
               'run_pagination': run_coverage, 'artifact_pagination': artifact_coverage,
               'runs': len(runs), 'artifacts': len(artifacts),
               'conclusions': dict(collections.Counter(r.get('conclusion') or r['status'] for r in runs)),
               'workflows': dict(collections.Counter(r['name'] for r in runs)),
               'attempts': sum(r.get('run_attempt', 1) for r in runs),
               'first_created': min(r['created_at'] for r in runs),
               'last_created': max(r['created_at'] for r in runs), 'api': dict(STATE)}
    save('inventory_summary.json', summary)
    print(json.dumps(summary, indent=1), flush=True)


def scan():
    runs = json.loads((OUT / 'runs.json').read_text())
    artifacts = json.loads((OUT / 'artifacts.json').read_text())
    tasks = []
    # Failed/cancelled attempts and recovered reruns first, then every retained successful run.
    for r in runs:
        for a in range(1, r.get('run_attempt', 1) + 1):
            priority = 0 if a < r.get('run_attempt', 1) else (1 if r.get('conclusion') not in ('success', 'skipped', 'neutral') else 3)
            tasks.append((priority, r['created_at'], 'logs', r['id'], a, r))
    diagnostic = re.compile(r'(?:evidence|runtime-|c2-|saved-|tango|probe|review|audit|result|report|failure|trace|test|bench)', re.I)
    by_id = {r['id']: r for r in runs}
    excluded_artifacts = []
    for a in artifacts:
        r = by_id.get(a.get('workflow_run', {}).get('id'))
        if not r or a.get('expired'):
            excluded_artifacts.append({'id': a['id'], 'name': a['name'], 'reason': 'expired or outside census'})
            continue
        if not diagnostic.search(a['name']) or re.search(r'(?:sources|snapshot|workspace|updated|bundle-(?:linux|macos))$', a['name']):
            excluded_artifacts.append({'id': a['id'], 'name': a['name'], 'reason': 'build/source artifact, not diagnostic'})
            continue
        priority = 2 if r.get('conclusion') not in ('success', 'skipped', 'neutral') else 4
        tasks.append((priority, r['created_at'], 'artifacts', a['id'], 0, {'artifact': a, 'run': r}))
    tasks.sort(key=lambda x: (x[0], x[1]), reverse=False)
    save('scan_plan.json', {'tasks': len(tasks), 'by_priority': dict(collections.Counter(t[0] for t in tasks)),
                            'excluded_artifacts': excluded_artifacts})
    print('PLAN', len(tasks), collections.Counter(t[0] for t in tasks), flush=True)

    def one(task):
        priority, date, kind, ident, attempt, meta = task
        item = {'kind': kind, 'id': ident, 'attempt': attempt, 'priority': priority,
                'created_at': date}
        if kind == 'logs':
            item.update(run_id=ident, workflow=meta['name'], conclusion=meta.get('conclusion'),
                        head_sha=meta['head_sha'], branch=meta['head_branch'])
            endpoint = '/actions/runs/' + str(ident) + '/attempts/' + str(attempt) + '/logs'
            key = str(ident) + '-' + str(attempt)
        else:
            item.update(run_id=meta['run']['id'], workflow=meta['run']['name'], name=meta['artifact']['name'],
                        conclusion=meta['run'].get('conclusion'), head_sha=meta['run']['head_sha'],
                        branch=meta['run']['head_branch'])
            endpoint = '/actions/artifacts/' + str(ident) + '/zip'
            key = str(ident)
        try:
            data = request(endpoint, binary=True)
            item['result'] = collect_archive(data, key, kind)
            item['status'] = 'scanned'
        except Exception as e:
            item.update(status='unavailable', error=str(e))
        save('index/' + kind + '-' + key + '.json', item)
        return item

    count = collections.Counter()
    with futures.ThreadPoolExecutor(max_workers=4) as pool:
        for n, item in enumerate(pool.map(one, tasks), 1):
            count[(item['kind'], item['status'])] += 1
            if n % 25 == 0 or n == len(tasks):
                print('SCANNED', n, '/', len(tasks), dict(STATE), str(count), flush=True)
    save('scan_summary.json', {'tasks': len(tasks), 'outcomes': {str(k): v for k,v in count.items()}, 'api': STATE})


def jobs():
    runs = json.loads((OUT / 'runs.json').read_text())
    tasks = [r for r in runs if r.get('conclusion') not in ('success', 'skipped', 'neutral') or r.get('run_attempt', 1)>1]
    results = []
    for i,r in enumerate(tasks):
        for attempt in range(1, r.get('run_attempt', 1)+1):
            try:
                data, coverage = pages('/actions/runs/' + str(r['id']) + '/attempts/' + str(attempt) + '/jobs', 'jobs')
                results.append({'run_id':r['id'],'attempt':attempt,'jobs':data,'coverage':coverage})
            except Exception as e:
                results.append({'run_id':r['id'],'attempt':attempt,'error':str(e)})
        if i % 20 == 0:
            save('jobs.json',results)
    save('jobs.json',results)

if __name__ == '__main__':
    {'inventory': inventory, 'scan': scan, 'jobs': jobs}[sys.argv[1]]()
