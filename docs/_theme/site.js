const siteScript = document.querySelector('script[data-search-url]');
const input = document.getElementById('docs-search-input');
const results = document.getElementById('search-results');
const toggle = document.querySelector('.guides-toggle');
const layout = document.querySelector('.docs-layout');
let searchIndex;

toggle?.addEventListener('click', () => {
  const open = toggle.getAttribute('aria-expanded') !== 'true';
  toggle.setAttribute('aria-expanded', String(open));
  layout.dataset.menuOpen = String(open);
});

function closeSearch() { results.hidden = true; }
document.querySelector('.docs-search')?.addEventListener('submit', (event) => event.preventDefault());
input?.addEventListener('input', async () => {
  const query = input.value.trim().toLowerCase();
  if (!query) { closeSearch(); return; }
  results.hidden = false;
  results.textContent = 'Searching…';
  try {
    searchIndex ??= fetch(siteScript.dataset.searchUrl).then((response) => {
      if (!response.ok) throw new Error('Search index unavailable.');
      return response.json();
    });
    const { docs } = await searchIndex;
    if (input.value.trim().toLowerCase() !== query) return;
    const terms = query.split(/\s+/);
    const matches = docs.filter((doc) => terms.every((term) => `${doc.title} ${doc.text}`.toLowerCase().includes(term)))
      .sort((a, b) => Number(b.title.toLowerCase().includes(query)) - Number(a.title.toLowerCase().includes(query)))
      .slice(0, 8);
    results.replaceChildren();
    if (!matches.length) { results.textContent = 'No matching sections. Try a different word.'; return; }
    for (const doc of matches) {
      const link = document.createElement('a');
      link.href = new URL(doc.location, new URL(siteScript.dataset.siteRoot, location.href)).href;
      const title = document.createElement('strong'); title.textContent = doc.title;
      const snippet = document.createElement('span'); snippet.textContent = doc.text.slice(0, 160);
      link.append(title, snippet);
      results.append(link);
    }
  } catch {
    if (input.value.trim().toLowerCase() === query) results.textContent = 'Search is unavailable. The guide navigation still works.';
    searchIndex = undefined;
  }
});
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeSearch();
  if (event.key === '/' && !event.ctrlKey && !event.metaKey && !['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement.tagName)) {
    event.preventDefault(); input?.focus();
  }
});
document.addEventListener('click', (event) => { if (!event.target.closest('.docs-search')) closeSearch(); });
