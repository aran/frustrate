// @ts-check
//
// The wasm-bindgen thread transform's contract, exercised in a real browser on
// the real artifact: two realms, one shared memory, one module.
//
// # Why this spec exists at all, given //threaded:app_web_threaded
//
// Every `#[bridge]` member in this demo is a member of the `Node` actor, so the
// app dispatches through the actor pump — one instance, its own private memory,
// thread 0 of it — and never calls `pool::spawn`. The app spec therefore covers
// the actor half of frustrate's post-pass support and *cannot* cover the pool
// half: N instances of one module on ONE shared memory, with wasm-bindgen's
// injected `__wbindgen_start` bootstrapping each non-first thread. No fixture in
// the repo combines wasm-bindgen with a pool-dispatched async fn (the threaded
// fixtures have no wasm-bindgen in their graphs, and adding it to
// tests/test_api would drag it into the main crate hub), so this spec is the
// only place that mechanism can be shown to work.
//
// It is deliberately NOT a second copy of the glue. It drives the artifact
// directly, in plain JS, so it pins the *transform's* contract — the surface
// that moves when an app bumps its own wasm-bindgen pin — rather than
// frustrate's code. The build-time half of the same job is
// //bazel/wasm_import_check's `--post-pass` mode; this is the runtime half.
//
// # What it asserts
//
//  1. Both realms run on the SAME `WebAssembly.Memory` — the one this harness
//     created — even though each realm's copy of `iroh_rust_bg.js` creates a
//     shared memory of its own at module scope. That is the whole fix: the
//     namespace handed to `WebAssembly.instantiate` is a copy with `memory`
//     overridden, and it reaches the generated shims because the module
//     re-exports the memory it imports and every shim reads
//     `wasm.memory.buffer` off the instance.
//  2. The worker is genuinely thread N, not a second thread 0: the transform
//     gave it its own stack (`__stack_alloc` nonzero, and zero on the first
//     thread, which keeps the linked stack pointer) and its own TLS block
//     (`__tls_base` different from the first thread's).
//  3. It did not wedge. The injected start's non-first-thread path takes a
//     `memory.atomic.wait32` on a temporary-stack lock; a worker that reached
//     it against a memory nobody else had initialised would park forever. A
//     reply means it came through.
//  4. Bytes the worker allocates and writes through the module's own allocator
//     are readable from the first thread at the same address — one memory, end
//     to end, not two that merely agree on layout.
//
// Nothing here contacts a third party and nothing here needs the relay: the
// module is fetched from this harness's own origin and never opens an endpoint.
const { test, expect } = require('@playwright/test');
const path = require('path');
const fs = require('fs');
const http = require('http');

const WEB_DIR = path.resolve(__dirname, '../bazel-bin/threaded/app_web_threaded_web');

// Its own port: playwright runs spec files in parallel workers, and the two
// other specs here each hold a listener of their own for the whole run.
const PORT = 3343;

const TYPES = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.wasm': 'application/wasm', '.json': 'application/json',
};

// The page under test, served from the origin the module comes from so the
// dynamic import() of the sidecar resolves as it does in the product.
const PAGE = `<!DOCTYPE html><html><head><meta charset="utf-8">
<title>thread transform mechanism</title></head><body>
<script src="/mech.js"></script></body></html>`;

// The first thread. Everything it does, frustrate's runtime does in
// runtime_web.dart; spelled out here so a failure names the step.
const MAIN = `
// The import namespaces the module declares besides the sidecar's. Stubs: this
// page never enters bridge code, so none of them can be reached — but
// instantiation resolves every import a module DECLARES, reachable or not.
function frustrateStubs() {
  const nope = (name) => () => { throw new Error('unexpected ' + name); };
  return {
    env: { __stack_chk_fail: nope('__stack_chk_fail') },
    frustrate: {
      post: nope('post'), panic: nope('panic'),
      schedule_drain: nope('schedule_drain'), spawn_worker: nope('spawn_worker'),
    },
  };
}

async function boot() {
  const memory = new WebAssembly.Memory(
      { initial: 256, maximum: 16384, shared: true });
  const module = await WebAssembly.compileStreaming(fetch('/iroh_rust.wasm'));
  const ns = await import('/iroh_rust_bg.js');

  // The sidecar made one of its own at module scope, and this realm is one of
  // two that will. Recording it is half the point: the assertion below is that
  // nothing ever reads it.
  const sidecarMemory = ns.memory;

  const imports = frustrateStubs();
  for (const i of WebAssembly.Module.imports(module)) {
    if (!['env', 'frustrate'].includes(i.module)) {
      imports[i.module] = Object.assign({}, ns, { memory });
    }
  }
  const instance = await WebAssembly.instantiate(module, imports);
  ns.__wbg_set_wasm(instance.exports);
  // No stack size: this is the first thread of this memory, which keeps the
  // stack pointer the linker gave it. It must also run to completion before any
  // other thread exists on this memory — it is what wins the data-init barrier
  // and takes thread id 0.
  instance.exports.__wbindgen_start();

  const worker = new Worker('/mech_worker.js');
  const reply = new Promise((resolve, reject) => {
    worker.onmessage = (e) => resolve(e.data);
    worker.onerror = (e) => reject(new Error('worker: ' + e.message));
    // A wedge is the failure this whole design exists to prevent, so it is
    // reported as one rather than as the spec's own timeout.
    setTimeout(() => reject(new Error(
        'the worker never replied: it is parked, which on a post-passed module '
        + 'means __wbindgen_start took the temporary-stack lock on a memory '
        + 'nobody else had initialised')), 30000);
  });
  worker.postMessage({ module, memory });
  const w = await reply;
  if (w.error) throw new Error('worker: ' + w.error);

  const bytes = new Uint8Array(memory.buffer, w.ptr, w.text.length);
  return {
    // (1) one memory, in both realms.
    mainMemoryIsOurs: instance.exports.memory === memory,
    workerMemoryIsOurs: w.memoryIsOurs,
    // ... and the sidecar's own is a different object that nothing reads.
    sidecarMemoryIsSpare: sidecarMemory !== memory,
    // (2) thread 0 keeps the linked stack; thread N gets its own.
    mainStackAlloc: instance.exports.__stack_alloc.value,
    workerStackAlloc: w.stackAlloc,
    tlsDiffers: instance.exports.__tls_base.value !== w.tlsBase,
    mainTlsNonZero: instance.exports.__tls_base.value !== 0,
    workerTlsNonZero: w.tlsBase !== 0,
    // (4) what the worker wrote, read here.
    readBack: new TextDecoder().decode(bytes.slice()),
    wrote: w.text,
  };
}

boot().then((r) => { window.__result = r; },
           (e) => { window.__result = { failed: String(e && e.stack || e) }; });
`;

// Thread N. Everything it does, the pool pump in frustrate.js does.
const WORKER = `
onmessage = async (e) => {
  const { module, memory } = e.data;
  try {
    const nope = (name) => () => { throw new Error('unexpected ' + name); };
    const imports = {
      env: { memory, __stack_chk_fail: nope('__stack_chk_fail') },
      frustrate: {
        post: nope('post'), panic: nope('panic'),
        schedule_drain: nope('schedule_drain'), spawn_worker: nope('spawn_worker'),
      },
    };
    // This realm's own copy of the sidecar, with its own useless memory.
    const ns = await import('/iroh_rust_bg.js');
    for (const i of WebAssembly.Module.imports(module)) {
      if (!['env', 'frustrate'].includes(i.module)) {
        imports[i.module] = Object.assign({}, ns, { memory });
      }
    }
    const instance = await WebAssembly.instantiate(module, imports);
    ns.__wbg_set_wasm(instance.exports);
    // 1 MiB, the pool's stack size — the transform's own default is 2 MiB, and
    // the argument is the only way to say otherwise.
    instance.exports.__wbindgen_start(1 << 20);

    // A real allocation through the module's allocator, on this thread's stack
    // and this thread's TLS. If either were wrong this is where it shows.
    const text = 'written on thread N: ' + Date.now();
    const enc = new TextEncoder().encode(text);
    const ptr = instance.exports.__wbindgen_malloc(enc.length, 1);
    new Uint8Array(instance.exports.memory.buffer, ptr, enc.length).set(enc);

    postMessage({
      memoryIsOurs: instance.exports.memory === memory,
      stackAlloc: instance.exports.__stack_alloc.value,
      tlsBase: instance.exports.__tls_base.value,
      ptr, text,
    });
  } catch (err) {
    postMessage({ error: String(err && err.stack || err) });
  }
};
`;

// COOP/COEP, from headers — the SharedArrayBuffer the module's memory is backed
// by exists only in a cross-origin-isolated page, and no browser flag is used.
function createServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
      res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
      const p = decodeURIComponent(req.url.split('?')[0]);
      const inline = { '/': PAGE, '/mech.js': MAIN, '/mech_worker.js': WORKER }[p];
      if (inline !== undefined) {
        res.setHeader('Content-Type', p === '/' ? 'text/html' : 'text/javascript');
        res.writeHead(200);
        return res.end(inline);
      }
      const f = path.join(WEB_DIR, p);
      if (!f.startsWith(WEB_DIR) || !fs.existsSync(f) || fs.statSync(f).isDirectory()) {
        res.writeHead(404);
        return res.end('not found');
      }
      res.setHeader('Content-Type', TYPES[path.extname(f)] || 'application/octet-stream');
      res.writeHead(200);
      fs.createReadStream(f).pipe(res);
    });
    server.on('error', reject);
    server.listen(PORT, '127.0.0.1',
        () => resolve({ server, url: `http://127.0.0.1:${PORT}` }));
  });
}

let server, serverUrl;

test.beforeAll(async () => {
  // bazel-bin points at the last configuration built, so a native build in
  // between silently repoints it and the artifacts vanish. Say what to build.
  if (!fs.existsSync(path.join(WEB_DIR, 'iroh_rust.wasm'))) {
    throw new Error(
        'missing build output: //threaded:app_web_threaded -> ' + WEB_DIR +
        '\n\nRun, from e2e/iroh_demo:\n' +
        '  bazel build //threaded:app_web_threaded\n' +
        '(it is tagged manual: threaded wasm needs the locally built +atomics ' +
        'std — `dart toolchain/custom_std/tool/build.dart` in the frustrate ' +
        'repo.)');
  }
  ({ server, url: serverUrl } = await createServer());
});

test.afterAll(() => { if (server) server.close(); });

test('two realms, one shared memory, one post-passed module', async ({ page }) => {
  const lines = [];
  page.on('console', (m) => lines.push(`[${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => lines.push(`[pageerror] ${e.message}`));

  await page.goto(serverUrl);
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);

  let r;
  try {
    await page.waitForFunction(() => window.__result, null, { timeout: 90000 });
    r = await page.evaluate(() => window.__result);
  } catch (err) {
    console.log('--- browser console ---\n' + lines.join('\n'));
    throw err;
  }
  expect(r.failed).toBeUndefined();

  // (1) One memory. Without the namespace override the worker instantiates
  // against its own realm's copy and this is the assertion that goes red.
  expect(r.mainMemoryIsOurs).toBe(true);
  expect(r.workerMemoryIsOurs).toBe(true);
  expect(r.sidecarMemoryIsSpare).toBe(true);

  // (2) Thread 0 keeps the linked stack pointer, so it never allocates one;
  // thread N is given one. Both get a TLS block, and they are not the same one.
  expect(r.mainStackAlloc).toBe(0);
  expect(r.workerStackAlloc).toBeGreaterThan(0);
  expect(r.mainTlsNonZero).toBe(true);
  expect(r.workerTlsNonZero).toBe(true);
  expect(r.tlsDiffers).toBe(true);

  // (4) The bytes crossed without being copied anywhere: one linear memory.
  expect(r.readBack).toBe(r.wrote);
});
