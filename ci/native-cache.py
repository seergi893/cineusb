#!/usr/bin/env python3
"""Seal/verify native-only CI cache bytes before refreshing Make sentinels.

An exact GitHub cache key binds these bytes to the NDK, all recipes and stable/release
configuration. Application objects and binaries are never included. ELF link evidence
is validated independently by the existing checker, and again on the final IPK.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import time

ROOT = Path(__file__).resolve().parent.parent
PREFIXES = ['vendor/ffmpeg-prefix', 'vendor/libass-prefix', 'vendor/sentry-native-prefix']
SENTINELS = ['vendor/ffmpeg-prefix/include/libavformat/avformat.h',
             'vendor/sentry-native-prefix/.built', 'pkg/libass-plx.so.0']
MANIFEST = '.cineusb-native-cache.json'

def files(root):
    paths = [p for name in PREFIXES for p in (root / name).rglob('*') if p.is_file() or p.is_symlink()]
    paths += [p for p in (root / 'pkg').glob('lib*-plx.so.*') if p.is_file() or p.is_symlink()]
    return sorted(set(paths))

def record(root):
    result = {}
    for path in files(root):
        if not path.resolve().is_relative_to(root.resolve()):
            raise ValueError('native cache contains an external symlink')
        result[path.relative_to(root).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    for name in SENTINELS:
        if name not in result:
            raise ValueError('native cache missing ' + name)
    return result

def verify(root, key):
    manifest = json.loads((root / MANIFEST).read_text())
    if manifest['key'] != key or manifest['files'] != record(root):
        raise ValueError('native cache key or bytes do not match')
    spec = importlib.util.spec_from_file_location('link_evidence', ROOT / 'ci/check-link-evidence.py')
    checker = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checker)
    libraries = [p for p in (root / 'pkg').glob('lib*-plx.so.*') if '.link.' not in p.name]
    for path in [*libraries, root / 'vendor/sentry-native-prefix/bin/sentry-crash']:
        checker.check_elf(path)
    # Checkout mtimes are new on every runner. Only verified, exact-key outputs
    # may be newer than those inputs. The application is always built afresh.
    stamp = time.time()
    import os
    for name in SENTINELS:
        os.utime(root / name, (stamp, stamp))
    for path in libraries:
        os.utime(path, (stamp, stamp))
    print('native-cache: exact key, complete byte manifest and ELF receipts verified')

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['seal', 'verify'])
    parser.add_argument('key')
    args = parser.parse_args()
    if args.mode == 'seal':
        (ROOT / MANIFEST).write_text(json.dumps({'key': args.key, 'files': record(ROOT)}, sort_keys=True))
    else:
        verify(ROOT, args.key)
