// A genuine writer-produced, synthetic Zarr fixture. Only the HTTP boundary
// can be fault-injected; successful metadata and chunks keep the writer bytes.
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';

export const readerFixture = JSON.parse(readFileSync(new URL('./fixtures/reader-regressions.json', import.meta.url), 'utf8'));

/** Serve original writer bytes over CORS/range-capable HTTP, with explicit fault overrides and
 * caller-owned shutdown.
 */
export async function fixtureStore(overrides = {}) {
  const files = new Map(Object.entries(readerFixture.files).map(([key, value]) => [key, Buffer.from(value, 'base64')]));
  const requests = [];
  const server = createServer((request, response) => {
    response.setHeader('Access-Control-Allow-Origin', '*');
    response.setHeader('Access-Control-Allow-Methods', 'GET,HEAD,OPTIONS');
    response.setHeader('Access-Control-Allow-Headers', 'Range');
    response.setHeader('Access-Control-Expose-Headers', 'Content-Length,Content-Range,Accept-Ranges');
    if (request.method === 'OPTIONS') { response.writeHead(204).end(); return; }
    const key = new URL(request.url, 'http://localhost').pathname.slice('/store.zarr/'.length);
    requests.push(key);
    if (overrides[key] === 'network-error') { request.socket.destroy(); return; }
    const data = files.get(key);
    const status = overrides[key] ?? (data ? 200 : 404);
    if (status !== 200) { response.writeHead(status).end(); return; }
    response.setHeader('Accept-Ranges', 'bytes');
    if (request.method === 'HEAD') { response.writeHead(200, { 'Content-Length': data.length }).end(); return; }
    const range = /^bytes=(\d+)-(\d*)$/.exec(request.headers.range ?? '');
    if (range) {
      const start = Number(range[1]), end = Math.min(Number(range[2] || data.length - 1), data.length - 1);
      if (start >= data.length) { response.writeHead(416, { 'Content-Range': `bytes */${data.length}` }).end(); return; }
      response.writeHead(206, { 'Content-Length': end - start + 1, 'Content-Range': `bytes ${start}-${end}/${data.length}` }).end(data.subarray(start, end + 1));
    } else response.writeHead(200, { 'Content-Length': data.length }).end(data);
  });
  await new Promise((done) => server.listen(0, '127.0.0.1', done));
  return {
    url: `http://127.0.0.1:${server.address().port}/store.zarr`, files, requests,
    close: () => new Promise((done) => { server.close(done); server.closeAllConnections(); }),
  };
}
