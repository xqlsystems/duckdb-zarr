// vtk.js channel-colored fluorescence viewer: vtkImageData + vtkVolumeMapper/vtkVolume
// reconstruct the selected SQL intensities without a per-cell segmentation overlay.
// vtkOrientationMarkerWidget for the XYZ gizmo, and vtkInteractorStyleTrackballCamera for orbit.
// The `{ render, clear, dispose }` contract and the dense-bounding-grid-from-SQL-rows approach
// (gaps stay exactly zero, never compacted) match visualizations.js's createScatter conventions;
// small DOM helpers below are intentionally duplicated from visualizations.js rather than shared,
// so that file's scatter code is never touched by this change.
import '@kitware/vtk.js/Rendering/Profiles/Volume';
import '@kitware/vtk.js/Rendering/Profiles/Geometry';

import vtkGenericRenderWindow from '@kitware/vtk.js/Rendering/Misc/GenericRenderWindow';
import vtkImageData from '@kitware/vtk.js/Common/DataModel/ImageData';
import vtkDataArray from '@kitware/vtk.js/Common/Core/DataArray';
import vtkVolume from '@kitware/vtk.js/Rendering/Core/Volume';
import vtkVolumeMapper from '@kitware/vtk.js/Rendering/Core/VolumeMapper';
import vtkColorTransferFunction from '@kitware/vtk.js/Rendering/Core/ColorTransferFunction';
import vtkPiecewiseFunction from '@kitware/vtk.js/Common/DataModel/PiecewiseFunction';
import vtkOutlineFilter from '@kitware/vtk.js/Filters/General/OutlineFilter';
import vtkMapper from '@kitware/vtk.js/Rendering/Core/Mapper';
import vtkActor from '@kitware/vtk.js/Rendering/Core/Actor';
import vtkInteractorStyleTrackballCamera from '@kitware/vtk.js/Interaction/Style/InteractorStyleTrackballCamera';
import vtkOrientationMarkerWidget from '@kitware/vtk.js/Interaction/Widgets/OrientationMarkerWidget';
import vtkAxesActor from '@kitware/vtk.js/Rendering/Core/AxesActor';
import { ColorMixPreset } from '@kitware/vtk.js/Rendering/Core/VolumeProperty/Constants';
import { buildVolumeGrid } from './volume_grid.js';

const BACKGROUND = '#08121f';
const BACKGROUND_RGB = [0.03, 0.07, 0.12];
const INK = '#c7d7e6';
const MUTED = '#8fa7bd';
function element(tag, style, text) {
  const node = document.createElement(tag);
  Object.assign(node.style, style);
  if (text !== undefined) node.textContent = text;
  return node;
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

function hexToRGB(hex) {
  const m = /^#?([0-9a-f]{6})$/i.exec(String(hex || '').trim());
  if (!m) return null;
  const value = parseInt(m[1], 16);
  return [((value >> 16) & 255) / 255, ((value >> 8) & 255) / 255, (value & 255) / 255];
}

/** Reconstruct only the supplied SQL voxels; missing coordinates remain zero.
 * spacing is XYZ in micrometers; channels supplies named display colors, and
 * channel identities come from the SQL c column, including mixed-channel queries.
 */
export function createVolume(container) {
  const { root, stage, footer, status } = frame(container, '3D fluorescence volume by channel', 'Drag to explore · scroll to zoom · double-click to fit.');

  const viewport = element('div', { position: 'absolute', inset: '0' });
  stage.appendChild(viewport);

  const controlsRow = element('div', { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '14px', marginTop: '10px', flexWrap: 'wrap' });
  const legend = element('div', { display: 'flex', flexWrap: 'wrap', gap: '14px' });
  legend.className = 'volume-channel-legend';
  legend.setAttribute('aria-label', 'Channel colors');
  legend.setAttribute('role', 'list');
  const rotationButton = element('button', { background: 'transparent', color: INK, border: '1px solid #426079', padding: '8px 12px', minHeight: '44px' });
  rotationButton.type = 'button';
  rotationButton.disabled = true;
  controlsRow.append(legend, rotationButton);
  footer.appendChild(controlsRow);

  const geometryCaption = element('div', { color: MUTED, marginTop: '6px' });
  geometryCaption.className = 'volume-geometry';
  footer.appendChild(geometryCaption);

  const orientation = element('div', { position: 'absolute', top: '8px', left: '10px', color: INK, pointerEvents: 'none', background: '#08121fcc', borderRadius: '4px', padding: '3px 6px', display: 'none' });
  orientation.className = 'volume-orientation';
  stage.appendChild(orientation);
  const empty = element('div', { position: 'absolute', inset: '0', display: 'grid', placeItems: 'center', textAlign: 'center', padding: '24px', color: MUTED, pointerEvents: 'none' }, 'Run a SQL query to view its 3D fluorescence voxels.');
  stage.appendChild(empty);

  let genericRenderWindow = null;
  let renderer = null;
  let interactor = null;
  let canvas = null;
  let volumeActor = null;
  let volumeMapper = null;
  let imageData = null;
  let outlineActor = null;
  let orientationWidget = null;
  let disposed = false;
  let unavailable = '';
  let stopObserving = () => {};
  let selectedView = 'isometric';
  let currentBounds = null; // { center: [x,y,z], radius }
  const events = new AbortController();
  const motionPreference = window.matchMedia('(prefers-reduced-motion: reduce)');
  let rotationEnabled = !motionPreference.matches;
  let inView = false;
  let dragging = false;
  let resumeAfter = 0;
  let rotationFrame = null;
  let lastRotationTime = 0;

  function stopRotation() {
    if (rotationFrame !== null) cancelAnimationFrame(rotationFrame);
    rotationFrame = null;
    lastRotationTime = 0;
    root.dataset.rotationState = 'empty';
  }

  function rotationState() {
    if (!currentBounds || disposed || !renderer) return 'empty';
    if (!rotationEnabled) return 'paused';
    if (!inView || document.hidden) return 'offscreen';
    return 'running';
  }

  function rotate(time) {
    rotationFrame = null;
    if (rotationState() !== 'running') return;
    if (!lastRotationTime) lastRotationTime = time;
    const elapsed = time - lastRotationTime;
    // A slow turn at no more than 15 draws/second. Never fight a drag or zoom.
    if (elapsed >= 1000 / 15) {
      if (!dragging && time >= resumeAfter) {
        renderer.getActiveCamera().azimuth(Math.min(elapsed, 150) * 0.006);
        renderer.resetCameraClippingRange();
        orientationWidget?.updateMarkerOrientation();
        render();
      }
      lastRotationTime = time;
    }
    rotationFrame = requestAnimationFrame(rotate);
  }

  function syncRotation() {
    stopRotation();
    const state = rotationState();
    root.dataset.rotationState = state;
    rotationButton.textContent = rotationEnabled ? 'Pause rotation' : 'Resume rotation';
    rotationButton.setAttribute('aria-pressed', String(rotationEnabled));
    rotationButton.disabled = !currentBounds;
    if (state === 'running') rotationFrame = requestAnimationFrame(rotate);
  }

  function suspendRotation() {
    resumeAfter = performance.now() + 1800;
  }

  rotationButton.addEventListener('click', () => {
    rotationEnabled = !rotationEnabled;
    syncRotation();
  }, { signal: events.signal });
  motionPreference.addEventListener('change', () => {
    rotationEnabled = !motionPreference.matches;
    syncRotation();
  }, { signal: events.signal });
  document.addEventListener('visibilitychange', syncRotation, { signal: events.signal });
  window.addEventListener('pointerup', () => { dragging = false; suspendRotation(); }, { signal: events.signal });
  window.addEventListener('pointercancel', () => { dragging = false; suspendRotation(); }, { signal: events.signal });
  const visibilityObserver = new IntersectionObserver(([entry]) => {
    inView = entry.isIntersecting;
    syncRotation();
  });
  visibilityObserver.observe(stage);
  syncRotation();

  function render() {
    if (!disposed && genericRenderWindow) {
      genericRenderWindow.getRenderWindow().render();
      if (renderer) root.dataset.cameraPosition = renderer.getActiveCamera().getPosition().join(',');
    }
  }

  function resize() {
    if (genericRenderWindow) genericRenderWindow.resize();
  }

  function cameraVectors(camera) {
    const pos = camera.getPosition();
    const fp = camera.getFocalPoint();
    const up = camera.getViewUp();
    const dir = [fp[0] - pos[0], fp[1] - pos[1], fp[2] - pos[2]];
    const dirLen = Math.hypot(...dir) || 1;
    const direction = dir.map((v) => v / dirLen);
    const right = [
      direction[1] * up[2] - direction[2] * up[1],
      direction[2] * up[0] - direction[0] * up[2],
      direction[0] * up[1] - direction[1] * up[0],
    ];
    const rightLen = Math.hypot(...right) || 1;
    return { pos, fp, up, direction, right: right.map((v) => v / rightLen) };
  }

  function panCamera(dx, dy) {
    if (!renderer || !currentBounds) return;
    const camera = renderer.getActiveCamera();
    const { pos, fp, up, right } = cameraVectors(camera);
    const step = Math.max(currentBounds.radius, 1e-6) * 0.09;
    const delta = [0, 1, 2].map((i) => right[i] * dx * step + up[i] * dy * step);
    camera.setPosition(pos[0] + delta[0], pos[1] + delta[1], pos[2] + delta[2]);
    camera.setFocalPoint(fp[0] + delta[0], fp[1] + delta[1], fp[2] + delta[2]);
    renderer.resetCameraClippingRange();
    render();
  }

  function applyView(view) {
    selectedView = view;
    if (!renderer || !currentBounds) return;
    const camera = renderer.getActiveCamera();
    const { center, radius } = currentBounds;
    const directions = { isometric: [1, 0.82, 1.1], xy: [0, 0, -1], xz: [0, -1, 0], yz: [-1, 0, 0] };
    const ups = { isometric: [0, 1, 0], xy: [0, 1, 0], xz: [0, 0, 1], yz: [0, 0, 1] };
    const d = directions[view] || directions.isometric;
    const u = ups[view] || ups.isometric;
    const dLen = Math.hypot(...d) || 1;
    const distance = Math.max(radius, 1e-6) * 3.2;
    camera.setFocalPoint(center[0], center[1], center[2]);
    camera.setPosition(center[0] + (d[0] / dLen) * distance, center[1] + (d[1] / dLen) * distance, center[2] + (d[2] / dLen) * distance);
    camera.setViewUp(...u);
    renderer.resetCamera();
    renderer.resetCameraClippingRange();
    if (orientationWidget) orientationWidget.updateMarkerOrientation();
    render();
    orientation.textContent = '3D fluorescence';
  }

  function startOrbit() {
    selectedView = 'custom';
    orientation.textContent = '3D fluorescence';
    suspendRotation();
  }

  try {
    genericRenderWindow = vtkGenericRenderWindow.newInstance({ background: BACKGROUND_RGB, listenWindowResize: false });
    genericRenderWindow.setContainer(viewport);
    renderer = genericRenderWindow.getRenderer();
    interactor = genericRenderWindow.getInteractor();
    interactor.setInteractorStyle(vtkInteractorStyleTrackballCamera.newInstance());
    canvas = genericRenderWindow.getApiSpecificRenderWindow().getCanvas();
    Object.assign(canvas.style, { touchAction: 'none', outlineOffset: '-3px' });
    canvas.tabIndex = 0;
    canvas.setAttribute('role', 'img');
    canvas.setAttribute('aria-label', 'Rotatable channel-colored 3D fluorescence volume. Drag to orbit, wheel to zoom, double-click to fit.');
    canvas.addEventListener('pointerdown', (event) => {
      canvas.focus({ preventScroll: true });
      dragging = true;
      if (event.button === 0) startOrbit();
    }, { signal: events.signal });
    canvas.addEventListener('wheel', suspendRotation, { passive: true, signal: events.signal });
    canvas.addEventListener('dblclick', () => applyView(selectedView === 'custom' ? 'isometric' : selectedView), { signal: events.signal });
    canvas.addEventListener('keydown', (event) => {
      if (!renderer || !currentBounds) return;
      suspendRotation();
      const camera = renderer.getActiveCamera();
      if (event.key === '0' || event.key === 'Home') applyView(selectedView === 'custom' ? 'isometric' : selectedView);
      else if (event.key === '+' || event.key === '=') { camera.dolly(1.2); renderer.resetCameraClippingRange(); render(); }
      else if (event.key === '-' || event.key === '_') { camera.dolly(0.84); renderer.resetCameraClippingRange(); render(); }
      else if (event.key === 'ArrowLeft') panCamera(-1, 0);
      else if (event.key === 'ArrowRight') panCamera(1, 0);
      else if (event.key === 'ArrowUp') panCamera(0, 1);
      else if (event.key === 'ArrowDown') panCamera(0, -1);
      else return;
      event.preventDefault();
    }, { signal: events.signal });
    const axes = vtkAxesActor.newInstance();
    orientationWidget = vtkOrientationMarkerWidget.newInstance({ actor: axes, interactor, parentRenderer: renderer });
    orientationWidget.setViewportCorner(vtkOrientationMarkerWidget.Corners.BOTTOM_RIGHT);
    orientationWidget.setViewportSize(0.13);
    orientationWidget.setMinPixelSize(60);
    orientationWidget.setMaxPixelSize(130);
    orientationWidget.setEnabled(true);
    stopObserving = observeSize(stage, resize);
    resize();
  } catch (error) {
    unavailable = `3D volume requires WebGL2: ${error.message}`;
    renderer = null;
    empty.textContent = unavailable;
    status.textContent = unavailable;
  }

  function buildOpacity(low, high) {
    const opacity = vtkPiecewiseFunction.newInstance();
    const upper = Math.max(high, low + 1e-4);
    opacity.addPoint(0, 0);
    opacity.addPoint(low, 0);
    // A linear opacity ramp accumulates background over hundreds of ray
    // samples, producing a veil. Keep dim signal nearly transparent while
    // retaining the brighter nuclear envelope and DNA features.
    opacity.addPoint(low + (upper - low) * 0.2, 0.005);
    opacity.addPoint((low + upper) / 2, 0.04);
    opacity.addPoint(upper, 0.55);
    opacity.addPoint(1, 0.8);
    return opacity;
  }

  function buildLegend(channels, channelIds) {
    legend.replaceChildren();
    for (const [index, channel] of channels.entries()) {
      const item = element('span', { display: 'inline-flex', alignItems: 'center', gap: '7px', opacity: channelIds.includes(index) ? '1' : '0.5' });
      item.setAttribute('role', 'listitem');
      item.dataset.active = String(channelIds.includes(index));
      item.dataset.color = channel.color;
      const swatch = element('span', { width: '10px', height: '10px', borderRadius: '50%', background: channel.color });
      swatch.setAttribute('aria-hidden', 'true');
      item.append(swatch, document.createTextNode(`${channel.name}${channelIds.includes(index) ? ' · shown' : ''}`));
      legend.appendChild(item);
    }
  }

  function releaseVolume() {
    if (volumeActor) { renderer?.removeVolume(volumeActor); volumeActor.delete(); volumeActor = null; }
    if (outlineActor) { renderer?.removeActor(outlineActor); outlineActor.getMapper()?.delete(); outlineActor.delete(); outlineActor = null; }
    volumeMapper?.delete();
    imageData?.getPointData().getScalars()?.delete();
    imageData?.delete();
    volumeMapper = null;
    imageData = null;
    currentBounds = null;
    rotationButton.disabled = true;
    stopRotation();
    delete root.dataset.channelColor;
    delete root.dataset.channelColors;
    delete root.dataset.channelIds;
    delete root.dataset.displayWindows;
    geometryCaption.textContent = '';
    delete geometryCaption.dataset.voxelDimensions;
    delete geometryCaption.dataset.physicalSize;
    delete geometryCaption.dataset.meshScale;
    legend.replaceChildren();
    orientation.style.display = 'none';
    render();
  }

  function clear() {
    if (disposed) return;
    releaseVolume();
    empty.style.display = 'grid';
    empty.textContent = unavailable || 'No SQL voxels to display.';
    status.textContent = unavailable || 'No SQL rows to display.';
  }

  function reject(message) {
    status.textContent = message;
    empty.textContent = message;
    empty.style.display = 'grid';
    return null;
  }

  return {
    render(table, { spacing = [1, 1, 1], channels = [] } = {}) {
      if (disposed) return null;
      clear();
      if (!renderer) return null;
      try {
        const grid = buildVolumeGrid(table, { spacing });
        const { data, dimensions, channelIds, statistics, count, skipped, physicalSize, scale: meshScale } = grid;
        const physicalSpacing = grid.spacing;
        const maximum = Math.max(...statistics.map((stats) => stats.maximum));
        const origin = [0, 1, 2].map((i) => -((dimensions[i] - 1) / 2) * physicalSpacing[i]);
        imageData = vtkImageData.newInstance();
        imageData.setDimensions(dimensions[0], dimensions[1], dimensions[2]);
        imageData.setSpacing(physicalSpacing[0], physicalSpacing[1], physicalSpacing[2]);
        imageData.setOrigin(origin[0], origin[1], origin[2]);
        imageData.getPointData().setScalars(vtkDataArray.newInstance({ name: 'intensity', values: data, numberOfComponents: channelIds.length }));

        volumeMapper = vtkVolumeMapper.newInstance();
        volumeMapper.setInputData(imageData);
        volumeMapper.setSampleDistance(Math.min(...physicalSpacing) * 0.5);
        volumeMapper.setMaximumSamplesPerRay(2000);
        volumeMapper.setAutoAdjustSampleDistances(false);
        volumeMapper.setImageSampleDistance(1);

        volumeActor = vtkVolume.newInstance();
        volumeActor.setMapper(volumeMapper);

        const property = volumeActor.getProperty();
        property.setIndependentComponents(true);
        property.setColorMixPreset(ColorMixPreset.ADDITIVE);
        for (const [component, channelId] of channelIds.entries()) {
          const { low, high } = statistics[component];
          const color = hexToRGB(channels[channelId]?.color) || [0.5, 0.86, 0.84];
          const transfer = vtkColorTransferFunction.newInstance();
          transfer.addRGBPoint(0, 0, 0, 0);
          transfer.addRGBPoint(low, ...color.map((value) => value * 0.3));
          transfer.addRGBPoint(high, ...color);
          transfer.addRGBPoint(1, ...color);
          property.setRGBTransferFunction(component, transfer);
          property.setScalarOpacity(component, buildOpacity(low, high));
          property.setScalarOpacityUnitDistance(component, Math.min(...physicalSpacing) * 1.5);
          property.setComponentWeight(component, 1);
        }
        property.setInterpolationTypeToLinear();
        property.setShade(false);
        renderer.addVolume(volumeActor);

        outlineActor = vtkActor.newInstance();
        const outlineFilter = vtkOutlineFilter.newInstance();
        outlineFilter.setInputData(imageData);
        const outlineMapper = vtkMapper.newInstance();
        outlineMapper.setInputConnection(outlineFilter.getOutputPort());
        outlineActor.setMapper(outlineMapper);
        outlineActor.getProperty().setColor(0.86, 0.93, 1);
        outlineActor.getProperty().setLighting(false);
        outlineActor.getProperty().setLineWidth(1.5);
        renderer.addActor(outlineActor);

        const bounds = imageData.getBounds();
        currentBounds = {
          center: [(bounds[0] + bounds[1]) / 2, (bounds[2] + bounds[3]) / 2, (bounds[4] + bounds[5]) / 2],
          radius: Math.hypot(bounds[1] - bounds[0], bounds[3] - bounds[2], bounds[5] - bounds[4]) / 2 || 1,
        };

        empty.style.display = 'none';
        root.dataset.channelColors = channelIds.map((id) => channels[id]?.color ?? '#80dcd5').join(',');
        root.dataset.channelIds = channelIds.join(',');
        root.dataset.channelColor = channelIds.length === 1 ? channels[channelIds[0]]?.color ?? '#80dcd5' : 'composite';
        root.dataset.displayWindows = JSON.stringify(statistics.map(({ id, low, high, maximum }) => ({ id, minimum: low * maximum, maximum: high * maximum })));
        buildLegend(channels, channelIds);
        geometryCaption.dataset.voxelDimensions = dimensions.join(',');
        geometryCaption.dataset.physicalSize = physicalSize.join(',');
        geometryCaption.dataset.meshScale = meshScale.join(',');
        geometryCaption.textContent = `${dimensions.join(' × ')} voxels (X×Y×Z) · ${physicalSize.map(format).join(' × ')} µm shown`;
        orientation.style.display = 'block';
        applyView(selectedView === 'custom' ? 'isometric' : selectedView);
        status.textContent = `${channelIds.length} channel${channelIds.length === 1 ? '' : 's'} · ${count.toLocaleString()} intensities · peak intensity ${format(maximum)}${skipped ? ` · ${skipped.toLocaleString()} rows skipped` : ''}${maximum === 0 ? ' · no signal' : ''}`;
        canvas.setAttribute('aria-label', `Rotatable fluorescence volume, ${dimensions.join(' by ')} voxels, ${count} SQL rows. Drag to orbit, wheel to zoom, double-click to fit.`);
        resize();
        render();
        syncRotation();
        return { count, skipped, dimensions, spacing: physicalSpacing, maximum, physicalSize, scale: meshScale, channelIds, statistics };
      } catch (error) {
        releaseVolume();
        return reject(`Unable to allocate the selected volume: ${error.message}`);
      }
    },
    clear,
    get viewState() {
      if (!renderer) return { view: selectedView, position: [0, 0, 0], up: [0, 1, 0], target: [0, 0, 0] };
      const camera = renderer.getActiveCamera();
      return { view: selectedView, position: camera.getPosition(), up: camera.getViewUp(), target: camera.getFocalPoint() };
    },
    dispose() {
      if (disposed) return;
      releaseVolume();
      disposed = true;
      stopObserving();
      visibilityObserver.disconnect();
      events.abort();
      if (orientationWidget) { orientationWidget.setEnabled(false); orientationWidget.delete(); orientationWidget = null; }
      if (genericRenderWindow) { genericRenderWindow.getRenderWindow().removeRenderer(renderer); genericRenderWindow.delete(); genericRenderWindow = null; }
      renderer = null;
      root.remove();
    },
  };
}
