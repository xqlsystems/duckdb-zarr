#!/usr/bin/env python3
"""Static dev server for the vgi-zarr browser demo.

The page reads Zarr stores straight from their buckets (they must send CORS
headers); this only serves the page, Haybarn-wasm and the worker build with
the headers a static host must add:

- COOP/COEP, so the page is cross-origin isolated (SharedArrayBuffer).
- HTTPS when --cert/--key are given: SharedArrayBuffer needs a secure
  context, and a LAN IP over plain HTTP is not one (only localhost is).

    python3 serve.py --haybarn <@haybarn/haybarn-wasm>/dist [--worker ../wasm/dist] \
        [--host 0.0.0.0 --port 8443 --cert cert.pem --key key.pem]
"""

import argparse
import http.server
import mimetypes
import os
import ssl
from functools import partial

mimetypes.add_type("application/wasm", ".wasm")
mimetypes.add_type("text/javascript", ".mjs")
mimetypes.add_type("text/javascript", ".js")


class Handler(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *a, mounts, **kw):
        self.mounts = mounts
        super().__init__(*a, **kw)

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.send_header("Cache-Control", "no-cache")
        super().end_headers()

    def translate_path(self, path):
        clean = path.split("?", 1)[0].split("#", 1)[0]
        for prefix, root in self.mounts:
            if clean.startswith(prefix):
                rel = clean[len(prefix):].lstrip("/")
                return os.path.join(root, *[p for p in rel.split("/") if p not in ("", ".", "..")])
        return super().translate_path(path)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8443)
    ap.add_argument("--haybarn", required=True, help="@haybarn/haybarn-wasm dist dir")
    ap.add_argument("--worker", default=os.path.join(here, "..", "wasm", "dist"))
    ap.add_argument("--cert")
    ap.add_argument("--key")
    a = ap.parse_args()
    mounts = [("/haybarn/", os.path.abspath(a.haybarn)), ("/vgi-zarr/", os.path.abspath(a.worker))]
    server = http.server.ThreadingHTTPServer(
        (a.host, a.port), partial(Handler, mounts=mounts, directory=here)
    )
    scheme = "http"
    if a.cert:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(a.cert, a.key)
        server.socket = ctx.wrap_socket(server.socket, server_side=True)
        scheme = "https"
    print(f"serving {scheme}://{a.host}:{a.port}/", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
