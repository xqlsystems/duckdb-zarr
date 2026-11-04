# Documentation website

The website publishes the existing Markdown guides and the browser biology
examples as one static site. The guide content does not need to be rewritten
or maintained in a second format.

## Structure

- `/` — the documentation overview, rendered from `docs/README.md`.
- `/anndata/` and `/ome-zarr/` — the existing biology guides.
- `/examples/climate/` and `/examples/geoscience/` — the Markdown field guides.
  There is no separate field-guide index page; use the direct sidebar links.
- `/biology/` — the first example page: interactive single-cell and 3D images.
  Its existing URL stays unchanged.
- The remaining guides stay in the sidebar, with search and section navigation.

The documentation and examples share the green-and-cream palette, serif
headings, wordmark, and top links: Documentation and GitHub.
The published biology page also uses the same sidebar, search, and mobile
Browse guides menu, with Biology marked as the active example.
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

Open `http://127.0.0.1:4178/` for the docs, then choose **Biology** under the sidebar's **Field guides** heading.
Use a different port if another preview is already listening on 4178.

The build script carries a pinned MkDocs dependency, invokes the existing Vite
build, and combines everything under ignored `site/`. It stages files only
temporarily; it does not edit the source Markdown. It does not download or
bundle scientific datasets. The biology app still fetches its original remote
sources only when someone opens the interactive page.

For biology, the builder stages `biology/index.md` using the compiled app's
main content and asset links. Its `biology.html` template extends the shared
`main.html`, so navigation comes from `mkdocs.yml` rather than a copied menu
or an iframe. The standalone Vite build remains unchanged.

`npm run dev` inside `demos/biology` remains useful for editing that app alone.
It is not the combined documentation preview, so its parent-documentation links
require the combined site above.

## Updating the site

- Edit the existing `.md` guide files as usual.
- Change sidebar organization in `mkdocs.yml`.
- Change the thin site shell in `docs/_theme/`.
- Set `example_title: true` in a Markdown example's front matter to use the
  same title typography and spacing as biology, with a green `title-accent`
  span on the second line.
- Keep demo-only rendering and SQL in `demos/biology/`.
- Add future example pages under `docs/examples/` and add direct links to the
  **Field guides** section in `mkdocs.yml`. The section heading is not a link.
  Separately built interactive apps can keep their own URLs; only those apps
  need additional build staging.

The site-only link hook keeps Markdown links useful in both contexts: links to
the demo README become links to the running examples, and links to source or
test files outside `docs/` go to GitHub. Code examples are left untouched.

## Deployment and checks

`.github/workflows/biology-demo-pages.yml` now publishes the entire `site/`
artifact, not just the demo. Its display name is **Documentation Pages**. It
reuses the checksum-pinned, browser-tested extension artifact from distribution
run `40233921996`; the deploy job does not rebuild the Rust extension. GitHub Pages
must use the GitHub Actions build method.

The workflow runs for documentation/example changes on `main`, or by manual
dispatch. The pinned artifact must still be available in the same repository;
if it expires, deployment fails instead of falling back to an untested binary.
The optional `run_id` input can point to another run, but its artifact must
have the pinned name and the digest in `demos/biology/extension.sha256`.
To adopt a new binary, browser-test it with the pinned DuckDB-Wasm package,
then update the workflow's run ID, artifact name and checksum together.
CI verifies the checksum and runs helper tests plus a real WASM-reader query
against a synthetic format fixture before uploading the Pages artifact.
A local build does not publish anything; changes still need to be reviewed
and merged before the workflow can deploy them.

`--skip-demo-build` reuses `demos/biology/dist` and requires both its
`index.html` and `zarr.duckdb_extension.wasm`. A copy of the extension under
`public/` cannot repair an incomplete existing build.

```sh
uv run --no-project --with mkdocs==1.6.1 python -m unittest discover -s scripts -p test_docsite.py
uv run scripts/build_docs.py
node demos/biology/test/docsite.browser.mjs site --layout-only
node demos/biology/test/docsite.browser.mjs site
```

The `--layout-only` check verifies identical sidebars, example-title styles,
search and mobile menus without starting the biology query engine. It blocks
only the app module, preserves its real compiled CSS/HTML, and is not a test
of scientific queries or rendering. The full command without that flag still
loads and checks the real datasets.

The full browser check follows docs → individual field guides → live biology app → docs,
exercises search and mobile navigation, checks generated internal links, and
loads the real remote data under a GitHub Pages-style project prefix. It uses
the existing Playwright installation (`npx playwright install chromium` from
`demos/biology` if needed).
