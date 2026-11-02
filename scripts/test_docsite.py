"""Regression checks for documentation build inputs and deployment pins.

Run with: uv run --no-project --with mkdocs==1.6.1 python -m unittest discover -s scripts -p test_docsite.py
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('build_docs', ROOT / 'scripts/build_docs.py')
assert SPEC is not None and SPEC.loader is not None
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


class ReusedBuildTests(unittest.TestCase):
    """Exercise the real builder against explicitly synthetic static-site inputs."""

    def test_reused_dist_requires_its_own_extension(self):
        """A public copy cannot repair a dist that was built without the extension."""
        with tempfile.TemporaryDirectory(prefix='docsite-test-') as temporary:
            root = Path(temporary)
            biology = root / 'demos/biology'
            (biology / 'dist').mkdir(parents=True)
            (biology / 'public').mkdir()
            (biology / 'dist/index.html').write_text('<h1>Static build test fixture</h1>')
            (biology / 'public/zarr.duckdb_extension.wasm').write_bytes(b'presence-test-only')
            (root / 'docs').mkdir()
            (root / 'docs/index.md').write_text('# Static build test fixture\n')
            (root / 'mkdocs.yml').write_text('site_name: Build test\n')
            output = root / 'site'
            errors = io.StringIO()
            with patch.multiple(BUILD, ROOT=root, BIOLOGY=biology), patch('sys.argv', ['build_docs.py', '--skip-demo-build', '--site-dir', str(output)]), contextlib.redirect_stderr(errors):
                with self.assertRaises(SystemExit) as raised:
                    BUILD.main()
            self.assertEqual(raised.exception.code, 2)
            self.assertIn('dist/zarr.duckdb_extension.wasm', errors.getvalue())
            self.assertFalse(output.exists(), 'Reject incomplete inputs before writing a site.')


    def test_reuse_needs_no_public_copy(self):
        """A complete built app is sufficient even without a source public directory."""
        with tempfile.TemporaryDirectory(prefix='docsite-test-') as temporary:
            root = Path(temporary)
            biology = root / 'demos/biology'
            (biology / 'dist').mkdir(parents=True)
            (biology / 'dist/index.html').write_text('<h1>Static build test fixture</h1>')
            extension = b'presence-test-only-not-a-real-wasm-module'
            (biology / 'dist/zarr.duckdb_extension.wasm').write_bytes(extension)
            (root / 'docs').mkdir()
            (root / 'docs/index.md').write_text('# Static build test fixture\n')
            (root / 'mkdocs.yml').write_text('site_name: Build test\n')
            output = root / 'site'
            with patch.multiple(BUILD, ROOT=root, BIOLOGY=biology), patch('sys.argv', ['build_docs.py', '--skip-demo-build', '--site-dir', str(output)]), contextlib.redirect_stdout(io.StringIO()):
                BUILD.main()
            self.assertEqual((output / 'biology/zarr.duckdb_extension.wasm').read_bytes(), extension)
            self.assertTrue((output / BUILD.MARKER).is_file())
            self.assertFalse((biology / 'public').exists())


class DeploymentPinTests(unittest.TestCase):
    """Execute the real workflow's resolver with only the GitHub CLI boundary replaced."""

    def test_default_run_does_not_follow_the_latest_successful_build(self):
        """A newer untested distribution cannot change the deployed extension."""
        import yaml

        workflow = yaml.safe_load((ROOT / '.github/workflows/biology-demo-pages.yml').read_text())
        step = next(step for step in workflow['jobs']['build']['steps'] if step.get('id') == 'source-run')
        with tempfile.TemporaryDirectory(prefix='pages-pin-test-') as temporary:
            directory = Path(temporary)
            gh = directory / 'gh'
            gh.write_text('#!/bin/sh\nprintf "99999999999\\n"\n')
            gh.chmod(0o755)
            outputs = directory / 'outputs'
            env = {
                **os.environ,
                **{key: value for key, value in step.get('env', {}).items() if '${{' not in str(value)},
                'GH_REPO': 'test-only/test-only',
                'REQUESTED_RUN': '',
                'GITHUB_OUTPUT': str(outputs),
                'PATH': str(directory) + os.pathsep + os.environ['PATH'],
            }
            result = subprocess.run(['bash', '-eo', 'pipefail', '-c', step['run']], env=env, text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('run_id=40233921996\n', outputs.read_text())


if __name__ == '__main__':
    unittest.main()
