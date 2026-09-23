# Serving the web glue under a strict CSP

Without help, the web runtime injects its JS glue at run time: an inline
`<script>` for the frame every bridge call goes through, and `blob:` URLs for
its workers. A strict Content-Security-Policy blocks all of these. So the same
glue is also shipped as a file, `frustrate.js`, which the page loads like any
other script and the runtime uses for its workers.

## Using it

```python
frustrate_web_glue(name = "frustrate.js")   # in the app's own package
```

List it in `flutter_web_app`'s `extra_web_assets` beside the wasm module, and
load it from `index.html` with a plain tag before `FrustrateWeb.init` runs:

```html
<script src="frustrate.js"></script>
```

No `async` or `defer`: the script has to have run by the time the runtime
starts. The script records its own URL, and the runtime creates workers from it.
If your bundler renames the file, set `globalThis.$frustrateGlueUrl` to its URL
yourself.

## The policy

`e2e/flutter_demo/web_csp.bzl` has the strictest policy frustrate supports, and
why each part is there. Two things it cannot say for you:

- **Your Rust's own network I/O needs `connect-src`.** A bridge crate that opens
  connections needs the page to allow hosts that neither the Dart, the HTML nor
  the bindings mention. Getting this wrong fails silently: the bridge
  initializes, the app renders, and the network events never arrive.
- **A policy that forbids `blob:` workers outright** breaks Flutter, not
  frustrate: the engine creates its skwasm render worker from a `blob:` URL.
  Either accept the engine's single-threaded fallback, or serve with
  `worker-src 'self'` and no cross-origin isolation.

## Why wasm-bindgen's JS is an asset, not glue

A bridge crate whose graph contains wasm-bindgen imports shims that only
wasm-bindgen's generated ES module can supply, and that module changes with
every build of the app. frustrate loads it by URL, as it does the `.wasm`
module, and the glue stays identical for every app.

Workers force this. The shims are needed wherever the module is instantiated,
and the workers are reached only through `postMessage`. A compiled
`WebAssembly.Module` can cross that boundary; an ES module namespace cannot be
cloned. The only handle to the generated module that survives is its URL.

Alternatives, and why each was rejected:

- **Inline the generated JS into a per-app copy of the glue.** It would reach
  the workers, but the glue would no longer be identical for every app.
- **Module-type workers**, which could `import` statically. That changes how
  every app's workers are created and gives up `importScripts`.
- **`--target no-modules`**, whose plain script a classic worker can load with
  `importScripts`. It is wasm-bindgen's legacy mode, with a global and a
  different handshake. It stays in reserve for an engine where dynamic
  `import()` fails inside a classic worker.
