// emscripten --js-library: synchronous ranged GET for HttpStore (src/http_store.rs),
// and the scan-stats side channel for the demo page.
// Runs on emscripten pthreads (Web Workers), where sync XHR is permitted.
addToLibrary({
  zarr_xhr_get__deps: ['malloc'],
  zarr_xhr_get: function (urlPtr, urlLen, rangePtr, rangeLen, outPtrPtr, outLenPtr) {
    // Never cache a heap view: another thread may grow memory while we block.
    var heap = function () { return new Uint8Array(wasmMemory.buffer); };
    var dec = new TextDecoder();
    var url = dec.decode(heap().slice(urlPtr, urlPtr + urlLen));
    var range = rangeLen ? dec.decode(heap().slice(rangePtr, rangePtr + rangeLen)) : '';
    var dv = function () { return new DataView(wasmMemory.buffer); };
    dv().setUint32(outPtrPtr, 0, true);
    dv().setUint32(outLenPtr, 0, true);
    try {
      var xhr = new XMLHttpRequest();
      xhr.open('GET', url, false);
      xhr.responseType = 'arraybuffer';
      if (range) xhr.setRequestHeader('Range', range);
      xhr.send(null);
      var body = new Uint8Array(xhr.response || new ArrayBuffer(0));
      if (body.length) {
        var p = _malloc(body.length);
        if (!p) return -1;
        heap().set(body, p);
        dv().setUint32(outPtrPtr, p, true);
        dv().setUint32(outLenPtr, body.length, true);
      }
      if (typeof globalThis.__zarrXhrCount === 'number') globalThis.__zarrXhrCount++;
      return xhr.status | 0;
    } catch (e) {
      console.error('[zarr_xhr_get]', url, range, e);
      return -1;
    }
  },

  // Post one scan's stats (JSON) to the page; see report_scan in src/read_zarr.rs.
  zarr_report_scan: function (ptr, len) {
    var json = new TextDecoder().decode(new Uint8Array(wasmMemory.buffer).slice(ptr, ptr + len));
    try {
      if (!globalThis.__zarrScanChannel) globalThis.__zarrScanChannel = new BroadcastChannel('vgi-zarr');
      globalThis.__zarrScanChannel.postMessage(json);
    } catch (e) {
      /* stats are a demo aid only */
    }
  },
});
