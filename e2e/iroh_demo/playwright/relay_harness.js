// @ts-check
//
// The origin server and the relay both browser specs here need, in one place.
//
// The indirection this file exists for: **both peers reach the relay through
// the page's own origin**, not through the relay's port. web/index.html's CSP
// is `connect-src 'self' https: wss:`, and a relay on another port matches none
// of those; serving it through the page's origin makes the dial `ws://` to the
// same host:port the page loaded from, which 'self' covers. The alternative was
// widening the CSP, but the specs are also the CSP proof, so that would mean
// damaging the artifact under test in order to run the test.
//
// Both peers must use the *same* URL string, because a peer dials the other
// peer's advertised home relay — so whatever origin is passed here has to be
// what both of them published.
const fs = require('fs');
const http = require('http');
const net = require('net');
const path = require('path');
const { spawn } = require('child_process');

const TYPES = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.wasm': 'application/wasm', '.json': 'application/json', '.png': 'image/png',
  '.css': 'text/css', '.otf': 'font/otf', '.ttf': 'font/ttf',
};

// The paths that belong to the relay rather than to the bundle.
//
// `/relay` is the websocket and `/ping` is the latency probe
// (iroh-relay/src/http.rs: RELAY_PATH, RELAY_PROBE_PATH). Both are public
// constants of the crate, so this list is upstream's, not a guess — and neither
// can collide with a Flutter asset, which is why matching by prefix is safe.
function isRelayPath(p) {
  return p === '/relay' || p.startsWith('/relay/') ||
         p === '/ping' || p.startsWith('/ping/');
}

// Forward a plain (non-upgrade) request to the relay and stream the reply back.
function proxyRequest(req, res, relayPort) {
  const upstream = http.request(
      { host: '127.0.0.1', port: relayPort, path: req.url, method: req.method, headers: req.headers },
      (up) => {
        res.writeHead(up.statusCode, up.headers);
        up.pipe(res);
      });
  upstream.on('error', () => { res.writeHead(502); res.end('relay unreachable'); });
  req.pipe(upstream);
}

/// Serve `root` on `port`, forwarding the relay's own paths to `relayPort`.
///
/// `isolate` adds COOP/COEP, which the threaded bundle requires and the
/// single-threaded one must not have: `SharedArrayBuffer` exists only in a
/// cross-origin-isolated page, and cross-origin isolation also constrains the
/// requests iroh makes, so it is opt-in per spec rather than a default here.
/// Extra `inline` entries (path -> {type, body}) are served ahead of the tree.
function createServer({ root, port, relayPort, isolate = false, inline = {} }) {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      if (isolate) {
        res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
        res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
      }
      let p = decodeURIComponent(req.url.split('?')[0]);

      // The relay's own paths, forwarded rather than served. Both are needed:
      // RELAY_PATH is the websocket, and RELAY_PROBE_PATH is the latency query
      // an Endpoint makes before it will accept a relay as its home. Serving a
      // 404 for /ping is enough to make an endpoint that connects fine never
      // come online — which is exactly how this first failed, on *both* peers,
      // including the native one, which is what ruled out the browser.
      if (isRelayPath(p)) return proxyRequest(req, res, relayPort);

      if (inline[p] !== undefined) {
        res.setHeader('Content-Type', inline[p].type);
        res.writeHead(200);
        return res.end(inline[p].body);
      }
      if (p === '/') p = '/index.html';
      const f = path.join(root, p);
      if (!f.startsWith(root) || !fs.existsSync(f) || fs.statSync(f).isDirectory()) {
        res.writeHead(404); return res.end('not found');
      }
      res.setHeader('Content-Type', TYPES[path.extname(f)] || 'application/octet-stream');
      res.writeHead(200);
      fs.createReadStream(f).pipe(res);
    });

    // The websocket leg. iroh forces the path to /relay and maps http -> ws
    // (iroh-relay/src/client.rs), and a raw socket pipe is enough: the upgrade
    // handshake and every frame after it pass through untouched.
    server.on('upgrade', (req, socket, head) => {
      if (!isRelayPath(req.url.split('?')[0])) return socket.destroy();
      const upstream = net.connect(relayPort, '127.0.0.1', () => {
        upstream.write(
            `${req.method} ${req.url} HTTP/1.1\r\n` +
            Object.entries(req.headers)
                .map(([k, v]) => `${k}: ${Array.isArray(v) ? v.join(', ') : v}\r\n`)
                .join('') +
            '\r\n');
        if (head && head.length) upstream.write(head);
        socket.pipe(upstream);
        upstream.pipe(socket);
      });
      const bin = () => { socket.destroy(); upstream.destroy(); };
      upstream.on('error', bin);
      socket.on('error', bin);
    });

    server.on('error', reject);
    // 127.0.0.1, not localhost: the app was built with a RELAY_URL naming this
    // literal, and a peer's home relay is identified by URL string. If the two
    // halves spell the same server differently they do not meet.
    server.listen(port, '127.0.0.1',
        () => resolve({ server, url: `http://127.0.0.1:${port}` }));
  });
}

// The relay every byte of these tests crosses.
//
// A browser cannot send UDP, so a web peer has no transport but a relay — this
// is not an optimisation, it is the only way a browser peer connects at all. It
// binds an ephemeral port and announces it on stdout; the caller then exposes it
// at its own origin (see the header).
function startRelay(relayDev) {
  const proc = spawn(relayDev, [], { stdio: ['pipe', 'pipe', 'pipe'] });
  let buf = '';
  // Everything the relay says, in the order it said it, for the failure dump.
  const lines = [];
  proc.stderr.on('data', (d) => lines.push(d.toString()));
  proc.stdout.on('data', (d) => lines.push(d.toString()));
  const port = new Promise((resolve, reject) => {
    const timer = setTimeout(
        () => reject(new Error(`relay_dev printed no url in 30s; said:\n${lines.join('')}`)),
        30000);
    proc.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/^relay: url http:\/\/127\.0\.0\.1:(\d+)/m);
      if (m) { clearTimeout(timer); resolve(Number(m[1])); }
    });
    proc.on('error', (e) => { clearTimeout(timer); reject(e); });
    proc.on('exit', (code) =>
        reject(new Error(`relay_dev exited early (${code}); said:\n${lines.join('')}`)));
  });
  return { proc, port, lines };
}

module.exports = { TYPES, createServer, isRelayPath, proxyRequest, startRelay };
