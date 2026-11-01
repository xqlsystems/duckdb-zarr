// Views own only their child element; mounting never replaces the parent's UI.
// The 3D fluorescence volume (+ labels) viewer lives in vtk_volume.js (createVolume), built on
// @kitware/vtk.js instead of three.js; this file now only owns the 2D canvas scatter plot.
const BACKGROUND = '#08121f';
const INK = '#c7d7e6';
const MUTED = '#8fa7bd';
const MAX_SCATTER_ROWS = 300_000;
const PALETTE = ['#62d9c6', '#8aa8ff', '#f6b86b', '#ef85ad', '#c0a0f5', '#a4d66d', '#65c6ef', '#f1d56c', '#df9390', '#89c8ab', '#dca4ed', '#b7c4d1'];

function element(tag, style, text) {
  const node = document.createElement(tag);
  Object.assign(node.style, style);
  if (text !== undefined) node.textContent = text;
  return node;
}

function number(value) {
  if (value == null || typeof value === 'boolean' || (typeof value === 'string' && !value.trim())) return NaN;
  if (!['number', 'string', 'bigint'].includes(typeof value)) return NaN;
  const result = Number(value);
  return Number.isFinite(result) && Math.abs(result) <= 1e150 ? result : NaN;
}

function format(value) {
  return Number.isFinite(value) ? value.toLocaleString(undefined, { maximumSignificantDigits: 5 }) : '—';
}

function frame(container, name, hint) {
  if (!container || typeof container.appendChild !== 'function') throw new TypeError('A visualization needs a DOM container.');
  const root = element('section', { display: 'flex', flexDirection: 'column', width: '100%', height: '100%', minHeight: '320px', minWidth: '0', overflow: 'hidden', background: BACKGROUND, color: INK, font: '12px/1.5 system-ui, sans-serif', borderRadius: '10px' });
  root.className = 'biology-visualization';
  root.setAttribute('aria-label', name);
  const stage = element('div', { position: 'relative', flex: '1 1 auto', minHeight: '240px', minWidth: '0', overflow: 'hidden' });
  const footer = element('div', { flex: '0 0 auto', padding: '10px 14px', borderTop: '1px solid #203346' });
  const instructions = element('div', { color: MUTED, marginBottom: '5px' }, hint);
  const status = element('div', { color: INK }, 'No SQL rows to display.');
  status.setAttribute('role', 'status');
  status.setAttribute('aria-live', 'polite');
  footer.append(instructions, status);
  root.append(stage, footer);
  container.appendChild(root);
  return { root, stage, footer, status };
}

function observeSize(target, callback) {
  if (typeof ResizeObserver !== 'undefined') {
    const observer = new ResizeObserver(callback);
    observer.observe(target);
    return () => observer.disconnect();
  }
  window.addEventListener('resize', callback);
  return () => window.removeEventListener('resize', callback);
}

function categoryColor(label) {
  let hash = 2166136261;
  for (let i = 0; i < label.length; i += 1) hash = Math.imul(hash ^ label.charCodeAt(i), 16777619);
  return PALETTE[(hash >>> 0) % PALETTE.length];
}

/** Canvas SQL scatter. Spatial defaults to image coordinates (Y downward).
 * Set invertY: false for Cartesian spatial coordinates. New results reset the view.
 * render(rows, { polygons }) accepts named source GeoJSON Polygon rings in the same XY coordinates.
 */
export function createScatter(container, { spatial = false, invertY = spatial } = {}) {
  const { root, stage, footer, status } = frame(container, spatial ? 'Spatial cell coordinates' : 'Cell embedding', 'Drag to pan · scroll to zoom · hover for cell details · double-click or 0 to fit. Keyboard: arrows, + / −.');
  const canvas = element('canvas', { display: 'block', width: '100%', height: '100%', touchAction: 'none', cursor: 'grab', outlineOffset: '-3px' });
  canvas.tabIndex = 0;
  canvas.setAttribute('role', 'img');
  canvas.setAttribute('aria-label', 'Interactive scatter plot. Drag or use arrow keys to pan; plus and minus zoom; zero resets.');
  const context = canvas.getContext('2d');
  if (!context) {
    root.remove();
    throw new Error('Canvas2D is unavailable.');
  }
  const tooltip = element('div', { display: 'none', position: 'absolute', zIndex: '2', pointerEvents: 'none', whiteSpace: 'pre-line', maxWidth: '260px', overflowWrap: 'anywhere', background: '#13283af5', color: '#eff7ff', padding: '9px 11px', border: '1px solid #426079', borderRadius: '7px', boxShadow: '0 4px 20px #0006' });
  tooltip.setAttribute('role', 'tooltip');
  stage.append(canvas, tooltip);
  const legend = element('div', { display: 'flex', flexWrap: 'wrap', gap: '5px 14px', marginTop: '8px', maxHeight: '88px', overflow: 'auto' });
  legend.setAttribute('aria-label', 'Cell category legend');
  legend.setAttribute('role', 'list');
  footer.appendChild(legend);
  const sourceSummary = element('div', { display: 'none', color: MUTED, marginTop: '6px' });
  sourceSummary.className = 'source-layer-summary';
  footer.appendChild(sourceSummary);

  let points = [];
  let sourcePolygons = [];
  let bounds = { cx: 0, cy: 0, dx: 1, dy: 1 };
  let width = 1;
  let height = 1;
  let zoom = 1;
  let panX = 0;
  let panY = 0;
  let drag = null;
  let disposed = false;
  let pending = 0;
  let hoverFrame = 0;
  let hoverPoint = null;
  let bins = new Map();
  const events = new AbortController();
  const on = (type, fn, options = {}) => canvas.addEventListener(type, fn, { ...options, signal: events.signal });
  const plot = () => ({ x: 58, y: 20, w: Math.max(1, width - 78), h: Math.max(1, height - 67) });

  function transform() {
    const box = plot();
    let sx = box.w / bounds.dx;
    let sy = box.h / bounds.dy;
    if (spatial) sx = sy = Math.min(sx, sy);
    return { ...box, sx: sx * zoom, sy: sy * zoom * (invertY ? 1 : -1), cx: box.x + box.w / 2 + panX, cy: box.y + box.h / 2 + panY };
  }

  function draw() {
    pending = 0;
    if (disposed) return;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const rect = stage.getBoundingClientRect();
    width = Math.max(1, rect.width);
    height = Math.max(1, rect.height);
    const pixelWidth = Math.max(1, Math.round(width * dpr));
    const pixelHeight = Math.max(1, Math.round(height * dpr));
    if (canvas.width !== pixelWidth || canvas.height !== pixelHeight) {
      canvas.width = pixelWidth;
      canvas.height = pixelHeight;
    }
    context.setTransform(dpr, 0, 0, dpr, 0, 0);
    context.fillStyle = BACKGROUND;
    context.fillRect(0, 0, width, height);
    bins = new Map();
    const t = transform();
    context.font = '11px system-ui, sans-serif';
    context.lineWidth = 1;
    for (let i = 0; i <= 4; i += 1) {
      const x = t.x + t.w * i / 4;
      const y = t.y + t.h * i / 4;
      context.strokeStyle = '#203346';
      context.beginPath();
      context.moveTo(x, t.y);
      context.lineTo(x, t.y + t.h);
      context.moveTo(t.x, y);
      context.lineTo(t.x + t.w, y);
      context.stroke();
      if (points.length || sourcePolygons.length) {
        context.fillStyle = MUTED;
        context.textAlign = 'center';
        context.fillText(format(bounds.cx + (x - t.cx) / t.sx), x, t.y + t.h + 18);
        context.textAlign = 'right';
        context.fillText(format(bounds.cy + (y - t.cy) / t.sy), t.x - 8, y + 4, 49);
      }
    }
    context.fillStyle = INK;
    context.textAlign = 'center';
    context.fillText(spatial ? 'Spatial X' : 'Embedding X', t.x + t.w / 2, height - 8);
    context.save();
    context.translate(13, t.y + t.h / 2);
    context.rotate(-Math.PI / 2);
    context.fillText(spatial ? `Spatial Y${invertY ? ' (image ↓)' : ''}` : 'Embedding Y', 0, 0);
    context.restore();
    if (!points.length && !sourcePolygons.length) {
      context.fillStyle = MUTED;
      context.fillText('No finite coordinates in the SQL result', t.x + t.w / 2, t.y + t.h / 2);
      return;
    }
    context.save();
    context.beginPath();
    context.rect(t.x, t.y, t.w, t.h);
    context.clip();
    const radius = points.length > 50_000 ? 1.5 : 2.5;
    context.globalAlpha = points.length > 50_000 ? 0.75 : 0.9;
    for (const point of points) {
      const x = t.cx + (point.x - bounds.cx) * t.sx;
      const y = t.cy + (point.y - bounds.cy) * t.sy;
      if (x < t.x - radius || x > t.x + t.w + radius || y < t.y - radius || y > t.y + t.h + radius) continue;
      point.screenX = x;
      point.screenY = y;
      context.fillStyle = point.color;
      context.beginPath();
      context.arc(x, y, radius, 0, Math.PI * 2);
      context.fill();
      const key = `${Math.floor(x / 24)},${Math.floor(y / 24)}`;
      if (!bins.has(key)) bins.set(key, []);
      bins.get(key).push(point);
    }
    // Source Polygon rings share the cell-coordinate transform, including Y direction.
    context.globalAlpha = 1;
    context.strokeStyle = '#f1d56c';
    context.fillStyle = '#f1d56c';
    context.lineWidth = 1.5;
    context.textAlign = 'center';
    for (const polygon of sourcePolygons) {
      context.beginPath();
      for (const ring of polygon.coordinates) {
        ring.forEach(([x, y], i) => {
          const px = t.cx + (x - bounds.cx) * t.sx;
          const py = t.cy + (y - bounds.cy) * t.sy;
          if (i === 0) context.moveTo(px, py);
          else context.lineTo(px, py);
        });
        context.closePath();
      }
      context.stroke();
      const x = t.cx + (polygon.labelX - bounds.cx) * t.sx;
      const y = t.cy + (polygon.labelY - bounds.cy) * t.sy;
      context.lineWidth = 3;
      context.strokeStyle = BACKGROUND;
      context.strokeText(polygon.name, x, y);
      context.lineWidth = 1.5;
      context.strokeStyle = '#f1d56c';
      context.fillText(polygon.name, x, y);
    }
    context.restore();
  }

  function requestDraw() {
    if (!disposed && !pending) pending = requestAnimationFrame(draw);
  }

  function hideTooltip() {
    tooltip.style.display = 'none';
    hoverPoint = null;
    cancelAnimationFrame(hoverFrame);
    hoverFrame = 0;
  }

  function reset() {
    zoom = 1;
    panX = panY = 0;
    hideTooltip();
    requestDraw();
  }

  function zoomAt(factor, x, y) {
    const box = plot();
    const next = Math.max(0.1, Math.min(1000, zoom * factor));
    const ratio = next / zoom;
    panX = (x - box.x - box.w / 2) * (1 - ratio) + panX * ratio;
    panY = (y - box.y - box.h / 2) * (1 - ratio) + panY * ratio;
    zoom = next;
    hideTooltip();
    requestDraw();
  }

  function showTooltip() {
    hoverFrame = 0;
    if (!hoverPoint || drag || disposed) return;
    const { x, y } = hoverPoint;
    let closest = null;
    let distance = 100;
    for (let dx = -1; dx <= 1; dx += 1) {
      for (let dy = -1; dy <= 1; dy += 1) {
        for (const point of bins.get(`${Math.floor(x / 24) + dx},${Math.floor(y / 24) + dy}`) || []) {
          const d = (point.screenX - x) ** 2 + (point.screenY - y) ** 2;
          if (d < distance) { distance = d; closest = point; }
        }
      }
    }
    if (!closest) { tooltip.style.display = 'none'; return; }
    const lines = [closest.label];
    if (closest.cellId != null) lines.push(`Cell: ${String(closest.cellId)}`);
    lines.push(`x: ${format(closest.x)}   y: ${format(closest.y)}`);
    if (Number.isFinite(closest.value)) lines.push(`Value: ${format(closest.value)}`);
    tooltip.textContent = lines.join('\n');
    tooltip.style.display = 'block';
    tooltip.style.left = `${Math.max(4, Math.min(x + 14, width - tooltip.offsetWidth - 5))}px`;
    tooltip.style.top = `${Math.max(4, Math.min(y + 14, height - tooltip.offsetHeight - 5))}px`;
  }

  on('pointerdown', event => {
    if (event.button !== 0 || drag) return;
    canvas.focus({ preventScroll: true });
    drag = { id: event.pointerId, x: event.clientX, y: event.clientY };
    canvas.setPointerCapture(event.pointerId);
    canvas.style.cursor = 'grabbing';
    hideTooltip();
  });
  on('pointermove', event => {
    if (drag && drag.id === event.pointerId) {
      panX += event.clientX - drag.x;
      panY += event.clientY - drag.y;
      drag.x = event.clientX;
      drag.y = event.clientY;
      requestDraw();
    } else if (!drag) {
      const rect = canvas.getBoundingClientRect();
      hoverPoint = { x: event.clientX - rect.left, y: event.clientY - rect.top };
      if (!hoverFrame) hoverFrame = requestAnimationFrame(showTooltip);
    }
  });
  function endDrag(event) {
    if (!drag || drag.id !== event.pointerId) return;
    if (canvas.hasPointerCapture(drag.id)) canvas.releasePointerCapture(drag.id);
    drag = null;
    canvas.style.cursor = 'grab';
  }
  on('pointerup', endDrag);
  on('pointercancel', endDrag);
  on('lostpointercapture', endDrag);
  on('pointerleave', hideTooltip);
  on('wheel', event => {
    event.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const delta = event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? height : 1);
    zoomAt(Math.exp(-Math.max(-500, Math.min(500, delta)) * 0.0015), event.clientX - rect.left, event.clientY - rect.top);
  }, { passive: false });
  on('dblclick', reset);
  on('keydown', event => {
    if (event.key === '0' || event.key === 'Home') reset();
    else if (event.key === '+' || event.key === '=') zoomAt(1.25, width / 2, height / 2);
    else if (event.key === '-' || event.key === '_') zoomAt(0.8, width / 2, height / 2);
    else if (event.key.startsWith('Arrow')) {
      if (event.key === 'ArrowLeft') panX += 35;
      else if (event.key === 'ArrowRight') panX -= 35;
      else if (event.key === 'ArrowUp') panY += 35;
      else if (event.key === 'ArrowDown') panY -= 35;
      hideTooltip();
      requestDraw();
    } else return;
    event.preventDefault();
  });
  const stopObserving = observeSize(stage, requestDraw);
  requestDraw();

  function clear() {
    if (disposed) return;
    points = [];
    sourcePolygons = [];
    sourceSummary.textContent = '';
    sourceSummary.style.display = 'none';
    bins.clear();
    if (drag && canvas.hasPointerCapture(drag.id)) canvas.releasePointerCapture(drag.id);
    drag = null;
    canvas.style.cursor = 'grab';
    legend.replaceChildren();
    status.textContent = 'No SQL rows to display.';
    reset();
  }

  return {
    render(rows, { polygons = [] } = {}) {
      if (disposed) return;
      clear();
      const categories = new Map();
      let skipped = 0;
      let examined = 0;
      let truncated = false;
      let minX = Infinity; let maxX = -Infinity;
      let minY = Infinity; let maxY = -Infinity;
      for (const row of rows || []) {
        if (examined >= MAX_SCATTER_ROWS) { truncated = true; break; }
        examined += 1;
        const x = number(row?.x); const y = number(row?.y);
        if (!Number.isFinite(x) || !Number.isFinite(y)) { skipped += 1; continue; }
        const label = row.label == null ? 'Unlabelled' : String(row.label);
        const color = categoryColor(label);
        points.push({ x, y, label, color, value: number(row.value), cellId: row.cell_id });
        categories.set(label, (categories.get(label) || 0) + 1);
        minX = Math.min(minX, x); maxX = Math.max(maxX, x);
        minY = Math.min(minY, y); maxY = Math.max(maxY, y);
      }
      for (const polygon of Array.isArray(polygons) ? polygons : []) {
        if (!Array.isArray(polygon?.coordinates)) continue;
        const coordinates = polygon.coordinates.map(ring => Array.isArray(ring)
          ? ring.map(pair => Array.isArray(pair) ? [number(pair[0]), number(pair[1])] : [NaN, NaN]) : []);
        // Reject malformed rings instead of inventing edges across missing coordinates.
        if (!coordinates.length || coordinates.some(ring => ring.length < 3 || ring.some(pair => pair.some(value => !Number.isFinite(value))))) continue;
        let left = Infinity; let right = -Infinity; let top = Infinity; let bottom = -Infinity;
        for (const [i, ring] of coordinates.entries()) {
          for (const [x, y] of ring) {
            minX = Math.min(minX, x); maxX = Math.max(maxX, x);
            minY = Math.min(minY, y); maxY = Math.max(maxY, y);
            if (i === 0) {
              left = Math.min(left, x); right = Math.max(right, x);
              top = Math.min(top, y); bottom = Math.max(bottom, y);
            }
          }
        }
        sourcePolygons.push({ name: String(polygon.name ?? 'Source layer'), coordinates, labelX: left / 2 + right / 2, labelY: top / 2 + bottom / 2 });
      }
      if (sourcePolygons.length) {
        sourceSummary.style.display = 'block';
        sourceSummary.textContent = `${sourcePolygons.length} source layer outline${sourcePolygons.length === 1 ? '' : 's'} (gold, named) · dots: cell center positions · source layers are not cell types.`;
      }
      let minValue = Infinity; let maxValue = -Infinity;
      for (const point of points) {
        if (Number.isFinite(point.value)) {
          minValue = Math.min(minValue, point.value);
          maxValue = Math.max(maxValue, point.value);
        }
      }
      const numeric = Number.isFinite(minValue);
      if (numeric) {
        for (const point of points) {
          const t = Number.isFinite(point.value) ? (point.value - minValue) / (maxValue - minValue || 1) : 0;
          point.color = `rgb(${Math.round(35 + 220 * t)},${Math.round(60 + 155 * t)},${Math.round(100 - 30 * t)})`;
        }
        const scale = element('div', { width: '100%', color: INK }, `Expression value: ${format(minValue)} – ${format(maxValue)} (selected cells)`);
        scale.appendChild(element('div', { height: '8px', marginTop: '5px', borderRadius: '4px', background: 'linear-gradient(to right, rgb(35,60,100), rgb(255,215,70))' }));
        legend.appendChild(scale);
      }
      if (points.length || sourcePolygons.length) {
        bounds = { cx: minX / 2 + maxX / 2, cy: minY / 2 + maxY / 2, dx: Math.max(maxX - minX || 1, 1e-100) * 1.12, dy: Math.max(maxY - minY || 1, 1e-100) * 1.12 };
      }
      for (const [label, count] of numeric ? [] : categories) {
        const item = element('span', { display: 'inline-flex', alignItems: 'center', gap: '6px', overflowWrap: 'anywhere' });
        item.setAttribute('role', 'listitem');
        const swatch = element('span', { width: '9px', height: '9px', flexShrink: '0', borderRadius: '50%', background: categoryColor(label) });
        swatch.setAttribute('aria-hidden', 'true');
        item.append(swatch, document.createTextNode(`${label} (${count.toLocaleString()})`));
        legend.appendChild(item);
      }
      status.textContent = `${points.length.toLocaleString()} cells · ${categories.size.toLocaleString()} categories${skipped ? ` · ${skipped.toLocaleString()} invalid rows skipped` : ''}${truncated ? ` · safety limit: first ${MAX_SCATTER_ROWS.toLocaleString()} SQL rows only` : ''}`;
      canvas.setAttribute('aria-label', `${spatial ? 'Spatial' : 'Embedding'} scatter, ${points.length} cells, ${categories.size} categories, ${sourcePolygons.length} source layer outlines. Drag or arrows to pan, scroll or plus/minus to zoom, zero to fit.`);
      requestDraw();
    },
    clear,
    dispose() {
      if (disposed) return;
      clear();
      disposed = true;
      events.abort();
      stopObserving();
      cancelAnimationFrame(pending);
      cancelAnimationFrame(hoverFrame);
      points = [];
      sourcePolygons = [];
      bins.clear();
      root.remove();
      canvas.width = canvas.height = 1;
    },
  };
}
