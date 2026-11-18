#!/usr/bin/env python3
"""Trusted scheduled runner for deps-update.py; one PR, followed by explicit CI dispatch."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

def run(*args, **kw):
    return subprocess.run(args, check=True, text=True, **kw)

if __name__ == '__main__':
    name, version = os.environ['DEPENDENCY'], os.environ['VERSION']
    if not all(re.fullmatch(r'[A-Za-z0-9_.-]+', value) for value in (name, version)):
        sys.exit('unexpected dependency identifier')
    branch = f'deps/{name}-{version}'
    prs = run('gh', 'pr', 'list', '--head', branch, '--state', 'open', '--json', 'number', capture_output=True)
    if json.loads(prs.stdout):
        print('Existing PR retained:', branch)
        sys.exit(0)
    closed = run('gh', 'pr', 'list', '--head', branch, '--state', 'closed', '--json', 'mergedAt', capture_output=True)
    if any(pr['mergedAt'] is None for pr in json.loads(closed.stdout)):
        print('Declined update retained; use update.hold to freeze the dependency:', branch)
        sys.exit(0)
    run('gh', 'auth', 'setup-git')
    run('git', 'switch', '-c', branch)
    with tempfile.TemporaryDirectory() as scratch:
        body = Path(scratch) / 'body.md'
        result = subprocess.run([sys.executable, 'ci/deps-update.py', '--bump', name, '--body-file', str(body)])
        if result.returncode not in (0, 3):
            sys.exit(result.returncode)
        if result.returncode == 3:
            with body.open('a') as out:
                out.write('\nCompatibility patch needs manual review; this draft must not merge.\n')
        run('git', 'add', 'ci/dependencies.json', 'THIRD-PARTY-NOTICES.md')
        run('git', '-c', 'user.name=github-actions[bot]', '-c', 'user.email=41898282+github-actions[bot]@users.noreply.github.com', 'commit', '-m', f'Deps: update {name} to {version}')
        run('git', 'push', 'origin', f'HEAD:refs/heads/{branch}')
        run('gh', 'pr', 'create', '--base', 'main', '--head', branch, '--draft', '--title', f'Deps: update {name} to {version}', '--body-file', str(body))
        # GITHUB_TOKEN pushes and PR creation do not trigger another workflow.
        run('gh', 'workflow', 'run', 'cineusb-ipk.yml', '--ref', branch)
