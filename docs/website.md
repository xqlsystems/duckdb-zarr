# Documentation website

The website publishes the existing Markdown guides and the browser biology
examples as one static site. The guide content does not need to be rewritten
or maintained in a second format.

## Structure

- `/` — the documentation overview, rendered from `docs/README.md`.
- `/anndata/` and `/ome-zarr/` — the existing biology guides.
- `/examples/` — the examples index, rendered from `docs/examples/index.md`.
- `/biology/` — the first example page: interactive single-cell and 3D images.
  Its existing URL stays unchanged.
- The remaining guides stay in the sidebar, with search and section navigation.

The documentation and examples share the green-and-cream palette, serif
headings, wordmark, and top links: Documentation, Examples, GitHub.
The examples also link directly to their corresponding format guides. Links
remain relative, including when GitHub Pages hosts the whole site below a
project prefix such as `/duckdb-zarr/`.

## Build and preview

Install the biology app's JS dependencies and obtain its matching WASM
extension using the [biology setup instructions](../demos/biology/README.md).
Then, from the repository root:

```sh
uv run scripts/build_docs.py
python3 -m http.server 4178 --bind 127.0.0.1 --directory site
```

Open `http://127.0.0.1:4178/` for the docs, then **Examples → For biologists**.
Use a different port if another preview is already listening on 4178.

The build script carries a pinned MkDocs dependency, invokes the existing Vite
build, and combines everything under ignored `site/`. It stages files only
temporarily; it does not edit the source Markdown. It does not download or
bundle scientific datasets. The biology app still fetches its original remote
sources only when someone opens the interactive page.

`npm run dev` inside `demos/biology` remains useful for editing that app alone.
It is not the combined documentation preview, so its parent-documentation links
require the combined site above.

## Updating the site

- Edit the existing `.md` guide files as usual.
- Change sidebar organization in `mkdocs.yml`.
- Change the thin site shell in `docs/_theme/`.
- Keep demo-only rendering and SQL in `demos/biology/`.
- Add future example pages under `docs/examples/`, link them from
  `docs/examples/index.md`, and add them to the **Examples** section in
  `mkdocs.yml`. Separately built interactive apps can keep their own URLs and
  link from the same index; only those apps need additional build staging.

The site-only link hook keeps Markdown links useful in both contexts: links to
the demo README become links to the running examples, and links to source or
test files outside `docs/` go to GitHub. Code examples are left untouched.

## Deployment and checks

`.github/workflows/biology-demo-pages.yml` now publishes the entire `site/`
artifact, not just the demo. Its display name is **Documentation Pages**. It
reuses a compatible extension artifact from the successful main distribution
pipeline; the deploy job does not rebuild the Rust extension. GitHub Pages
must use the GitHub Actions build method.

The workflow runs for documentation/example changes on `main`, or by manual
dispatch. A successful extension distribution run must already exist in the
same repository; its artifact is reused rather than rebuilt. A local build does
not publish anything; changes still need to be
reviewed and merged before the workflow can deploy them.

```sh
uv run scripts/build_docs.py
node demos/biology/test/docsite.browser.mjs site
```

The browser check follows docs → examples index → live biology app → docs,
exercises search and mobile navigation, checks generated internal links, and
loads the real remote data under a GitHub Pages-style project prefix. It uses
the existing Playwright installation (`npx playwright install chromium` from
`demos/biology` if needed).
