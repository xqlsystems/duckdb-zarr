"""Site-only link adapters; the existing Markdown remains useful on GitHub."""

import posixpath
import re
from pathlib import PurePosixPath
from urllib.parse import quote, urlsplit

from mkdocs.utils import get_relative_url


def on_page_markdown(markdown, *, page, config, files):
    """Keep demo links on the site and send repository-only files to GitHub."""
    def replace(match):
        """Adapt one repository-relative Markdown link, retaining its label and fragment."""
        label, href = match.groups()
        parts = urlsplit(href)
        target = posixpath.normpath(str(PurePosixPath('docs') / PurePosixPath(page.file.src_uri).parent / parts.path))
        if target == 'demos/biology/README.md':
            destination = get_relative_url('biology/index.md', page.file.src_uri)
        elif not target.startswith('docs/') and not target.startswith('../'):
            destination = f"{config.repo_url.rstrip('/')}/blob/main/{quote(target, safe='/')}"
        else:
            return match.group(0)
        if parts.fragment:
            destination += f'#{parts.fragment}'
        return f'[{label}]({destination})'

    # Only adapt actual relative Markdown links, never code-fenced examples.
    segments = re.split(r'(^```[^\n]*\n.*?^```\s*$)', markdown, flags=re.MULTILINE | re.DOTALL)
    for index in range(0, len(segments), 2):
        segments[index] = re.sub(r'(?<!!)\[([^\]]+)\]\((\.\./[^\s)]+)\)', replace, segments[index])
    return ''.join(segments)
