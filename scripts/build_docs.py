# /// script
# requires-python = ">=3.11"
# dependencies = ["mkdocs==1.6.1"]
# ///
"""Build existing guides and the biology app into one static documentation site.

Run with `uv run scripts/build_docs.py`. No Markdown sources are rewritten and
no scientific datasets are downloaded during the build.
"""

import argparse
from html.parser import HTMLParser
import logging
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BIOLOGY = ROOT / 'demos/biology'
MARKER = '.duckdb-zarr-docsite'


class BiologyHTML(HTMLParser):
    """Retain the compiled app's main content and assets, not its standalone shell."""

    def __init__(self):
        super().__init__(convert_charrefs=False)
        self.body = []
        self.assets = []
        self.in_main = False
        self.main_count = 0

    def handle_starttag(self, tag, attrs):
        """Collect main children and external Vite assets without rebuilding their attributes."""
        attributes = dict(attrs)
        markup = self.get_starttag_text()
        assert markup is not None
        if tag == 'main':
            self.in_main = True
            self.main_count += 1
        elif self.in_main:
            self.body.append(markup)
        elif tag == 'link' and attributes.get('rel') == 'stylesheet':
            self.assets.append(markup)
        elif tag == 'script' and attributes.get('type') == 'module' and attributes.get('src'):
            self.assets.append(markup + '</script>')

    def handle_endtag(self, tag):
        """Exclude the main wrapper while preserving its children's closing tags."""
        if tag == 'main':
            self.in_main = False
        elif self.in_main:
            self.body.append(f'</{tag}>')

    def handle_startendtag(self, tag, attrs):
        """Preserve self-closing markup using the same collection rules as start tags."""
        self.handle_starttag(tag, attrs)

    def handle_data(self, data):
        """Keep application text but discard text from the standalone document shell."""
        if self.in_main:
            self.body.append(data)

    def handle_entityref(self, name):
        """Preserve named HTML entities rather than decoding/re-escaping application text."""
        self.handle_data(f'&{name};')

    def handle_charref(self, name):
        """Preserve numeric HTML entities in the application body."""
        self.handle_data(f'&#{name};')


def main():
    """Build the staged static site without editing guides or clearing foreign output directories."""
    logging.basicConfig(level=logging.INFO, format='%(levelname)s: %(message)s')
    from mkdocs.commands.build import build
    from mkdocs.config import load_config

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--site-dir', type=Path, default=ROOT / 'site')
    parser.add_argument('--skip-demo-build', action='store_true', help='Reuse an already-built demos/biology/dist.')
    args = parser.parse_args()
    output = args.site_dir.resolve()
    if output.exists() and any(output.iterdir()) and not (output / MARKER).exists():
        parser.error(f'Refusing to clear non-docsite output directory: {output}')
    if not args.skip_demo_build:
        if not (BIOLOGY / 'public/zarr.duckdb_extension.wasm').is_file():
            parser.error('Missing biology WASM extension. Follow demos/biology/README.md before building the combined site.')
        subprocess.run(['npm', 'run', 'build'], cwd=BIOLOGY, check=True, timeout=120)
    for name in ('index.html', 'zarr.duckdb_extension.wasm'):
        if not (BIOLOGY / 'dist' / name).is_file():
            parser.error(f'Missing biology dist/{name}. Build it first or omit --skip-demo-build.')
    if (BIOLOGY / 'dist/data').exists():
        parser.error('The biology app must not ship a local scientific dataset.')
    app = BiologyHTML()
    app.feed((BIOLOGY / 'dist/index.html').read_text())
    app.close()
    if app.main_count != 1 or app.in_main:
        parser.error('Biology dist/index.html must contain one complete main element.')

    with tempfile.TemporaryDirectory(prefix='duckdb-zarr-docs-', dir=os.environ.get('TMPDIR')) as temporary:
        staged_docs = Path(temporary) / 'docs'
        shutil.copytree(ROOT / 'docs', staged_docs, ignore=shutil.ignore_patterns('_theme', '_hooks.py', '__pycache__'))
        shutil.copytree(BIOLOGY / 'dist', staged_docs / 'biology')
        # Generate a real MkDocs page at the same URL; only its app content and
        # compiled assets vary. Navigation, search and mobile controls stay shared.
        (staged_docs / 'biology/index.html').unlink()
        (staged_docs / 'biology/index.md').write_text(
            '---\ntitle: duckdb-zarr for biologists\ntemplate: biology.html\n---\n'
            'Explore single-cell RNA-seq and multichannel 3D fluorescence with SQL.\n'
        )
        logo_sources = staged_docs / 'biology/logos/README.md'
        if logo_sources.exists():
            # Preserve attribution as a downloadable text asset, not an extra
            # guide page accidentally added to the documentation search index.
            logo_sources.rename(logo_sources.with_name('SOURCES.txt'))
        config = load_config(str(ROOT / 'mkdocs.yml'), docs_dir=str(staged_docs), site_dir=str(output))
        config.extra['biology'] = {'body': ''.join(app.body), 'assets': '\n'.join(app.assets)}
        output.mkdir(parents=True, exist_ok=True)
        try:
            build(config)
        finally:
            # Keep failed generated output recognizable, so a strict-build
            # warning does not make the next safe retry look like foreign data.
            (output / MARKER).write_text('Generated by scripts/build_docs.py\n')
    (output / '.nojekyll').touch()
    print(f'Documentation: {output / "index.html"}')
    print(f'Biologists examples: {output / "biology/index.html"}')
    print(f'Preview: python3 -m http.server 4178 --bind 127.0.0.1 --directory {output}')


if __name__ == '__main__':
    main()
