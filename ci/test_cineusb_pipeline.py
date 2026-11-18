"""CineUSB CI must fail closed and reuse only configuration-scoped build outputs."""
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

class Pipeline(unittest.TestCase):
    def test_dependency_failure_cannot_be_reported_as_success(self):
        workflow = (ROOT / '.github/workflows/cineusb-ipk.yml').read_text()
        self.assertNotIn('available=false', workflow)
        self.assertNotIn("if: steps.usb-deps.outputs.available", workflow)
        self.assertIn("needs.usb-smoke.result == 'success'", workflow)

    def test_architecture_and_notices_are_required(self):
        workflow = (ROOT / '.github/workflows/cineusb-ipk.yml').read_text()
        for script in ['check-module-layers.py', 'gen-third-party-notices.py --check', 'test_cineusb_pipeline.py']:
            self.assertIn(script, workflow)

    def test_cineusb_build_excludes_unused_uri_and_diagnostics(self):
        workflow = (ROOT / '.github/workflows/cineusb-ipk.yml').read_text()
        self.assertIn('PLAYBACK_BACKENDS: feed', workflow)
        self.assertIn('--no-default-features --features hostsim,local-usb', workflow)

    def test_notices_execute_only_the_trusted_base(self):
        workflow = (ROOT / '.github/workflows/dependabot-notices.yml').read_text()
        runner = (ROOT / 'ci/dependabot-notices.py').read_text()
        self.assertIn("pull_request.user.login == 'dependabot[bot]'", workflow)
        self.assertIn('head.repo.full_name == github.repository', workflow)
        self.assertIn('ref: ${{ github.event.pull_request.base.sha }}', workflow)
        self.assertNotIn('shell=True', runner)
        self.assertIn("str(ROOT / 'ci/gen-third-party-notices.py')", runner)
        self.assertIn('--force-with-lease=refs/heads/', runner)

    def test_cache_cannot_reuse_application_or_another_configuration(self):
        workflow = (ROOT / '.github/workflows/cineusb-ipk.yml').read_text()
        self.assertIn('ci/cargo-cache.py rust-modules/target', workflow)
        self.assertIn('ci/native-cache.py verify', workflow)
        self.assertIn('ci/native-cache.py seal', workflow)
        self.assertIn('ci/patches/**', workflow)
        native = workflow[workflow.index('      - name: Cache verified native dependencies'):workflow.index('      - name: Verify restored native cache')]
        self.assertNotIn('restore-keys:', native)
        self.assertNotIn('pkg/plxnative', native)

if __name__ == '__main__':
    unittest.main()
