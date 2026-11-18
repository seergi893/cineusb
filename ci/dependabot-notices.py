#!/usr/bin/env python3
"""Read same-repository Dependabot files as data; execute only trusted base scripts."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent

def run(*args, **kw):
    return subprocess.run(args, check=True, text=True, **kw)

if __name__ == '__main__':
    sha, ref = os.environ['HEAD_SHA'], os.environ['HEAD_REF']
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        sys.exit('invalid head SHA')
    run('git', 'check-ref-format', 'refs/heads/' + ref)
    target = Path(os.environ['RUNNER_TEMP']) / 'notices-input'
    run('gh', 'auth', 'setup-git')
    run('git', 'fetch', 'origin', sha)
    run('git', 'worktree', 'add', '--detach', str(target), sha)
    toolchain = tomllib.loads((target / 'rust-modules/rust-toolchain.toml').read_text())['toolchain']['channel']
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', toolchain):
        sys.exit('only official numbered Rust releases may regenerate notices')
    script = str(ROOT / 'ci/gen-third-party-notices.py')
    run('rustup', 'toolchain', 'install', toolchain, '--profile', 'minimal', '--component', 'rust-src')
    run(sys.executable, script, '--root', str(target), '--derive-toolchain')
    run(sys.executable, script, '--root', str(target))
    run('git', '-C', str(target), 'add', 'THIRD-PARTY-NOTICES.md', 'ci/third-party-crates.json')
    changed = subprocess.run(['git', '-C', str(target), 'diff', '--cached', '--quiet']).returncode
    if changed:
        run('git', '-C', str(target), '-c', 'user.name=github-actions[bot]', '-c', 'user.email=41898282+github-actions[bot]@users.noreply.github.com', 'commit', '-m', 'Deps: regenerate corresponding third-party notices')
        # Expected old head prevents overwriting a concurrent bot update.
        run('git', '-C', str(target), 'push', '--force-with-lease=refs/heads/' + ref + ':' + sha, 'origin', 'HEAD:refs/heads/' + ref)
        run('gh', 'workflow', 'run', 'cineusb-ipk.yml', '--ref', ref)
