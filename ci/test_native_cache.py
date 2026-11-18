"""Corrupted/stale native caches must fail before any Make sentinel is refreshed."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('native_cache', ROOT / 'ci/native-cache.py')
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)

class NativeCache(unittest.TestCase):
    def fixture(self, root):
        for name in cache.SENTINELS:
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'header')
        for name in ['pkg/libass-plx.so.0', 'vendor/sentry-native-prefix/bin/sentry-crash']:
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'\x7fELFfixture')
            record = {'archive_excluded': True, 'elf_sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
            for suffix, field, value in [('.link.map', 'map_sha256', b'LOAD library'), ('.link.trace', 'trace_sha256', b'library')]:
                Path(str(path) + suffix).write_bytes(value)
                record[field] = hashlib.sha256(value).hexdigest()
            Path(str(path) + '.link.json').write_text(json.dumps(record))
        (root / cache.MANIFEST).write_text(json.dumps({'key': 'known', 'files': cache.record(root)}))

    def test_verified_cache_is_accepted(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            cache.verify(root, 'known')

    def test_changed_headers_are_rejected_before_refresh(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            (root / cache.SENTINELS[0]).write_bytes(b'changed')
            with self.assertRaises(ValueError):
                cache.verify(root, 'known')

    def test_other_configuration_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            with self.assertRaises(ValueError):
                cache.verify(root, 'different')

    def test_missing_library_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            (root / 'pkg/libass-plx.so.0').unlink()
            with self.assertRaises(ValueError):
                cache.verify(root, 'known')

    def test_a_byte_manifest_does_not_replace_elf_verification(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            (root / 'pkg/libass-plx.so.0').write_bytes(b'not an ELF')
            (root / cache.MANIFEST).write_text(json.dumps({'key': 'known', 'files': cache.record(root)}))
            with self.assertRaises(ValueError):
                cache.verify(root, 'known')

    def test_application_binaries_are_never_part_of_the_native_cache(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root)
            (root / 'pkg/plxnative').write_bytes(b'application')
            self.assertNotIn('pkg/plxnative', cache.record(root))

if __name__ == '__main__':
    unittest.main()
