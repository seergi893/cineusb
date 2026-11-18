#!/usr/bin/env python3
"""Reuse the existing Cargo seed's safety policy: never trust cached path-crate outputs."""
import importlib.util
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('cargo_seed', ROOT / 'tools/cargo-seed.py')
seed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(seed)

if __name__ == '__main__':
    names = seed.local_packages(str(ROOT))
    if not names:
        sys.exit('cargo-cache: cannot identify path packages; refusing the cache')
    for value in sys.argv[1:]:
        target = (ROOT / value).resolve()
        if not target.is_relative_to(ROOT / 'rust-modules'):
            sys.exit('cargo-cache: target must remain inside rust-modules')
        seed.strip_app(str(target), seed.local_matcher(names))
    print('cargo-cache: workspace outputs removed; registry artifacts remain fingerprint-checked by Cargo')
