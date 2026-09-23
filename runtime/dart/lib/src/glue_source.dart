/// The web glue, embedded for the no-CSP fallback (inline `<script>` and
/// `blob:` workers). Canonical source: `lib/src/js/frustrate.js` — the file
/// served as a static asset for strict-CSP pages. KEEP BYTE-IDENTICAL to
/// that file; `test/glue_source_test.dart` pins the equality.
///
/// This library deliberately imports nothing (no `dart:js_interop`), so the
/// pin test can run on the VM.
library;

const frustrateGlueSource = r'''// frustrate's web glue: the one JavaScript file of the runtime. Served as a
// static asset for strict-CSP pages (script-src without 'unsafe-inline',
// worker-src without blob:) via a plain <script src="frustrate.js"> tag —
// no async/defer, so it runs at parse time, before any init — and embedded
// verbatim in the Dart runtime as the no-CSP fallback (inline <script> +
// blob: workers). The same file is both the main-thread script and the
// worker script; each context takes its branch below.
//
// KEEP BYTE-IDENTICAL to lib/src/glue_source.dart (pinned by
// test/glue_source_test.dart).
(() => {
  // The three host answers std needs, in one place, because two callers want
  // them: the wasip1 shim below (where std asks through `wasi_snapshot_preview1`)
  // and the `frustrate.*` facility imports (where a custom_std built with
  // --facilities asks us directly). One implementation, so the two branches
  // cannot answer a question differently.
  const hostFacilities = (getMemory) => {
    const bytes = (ptr, len) => new Uint8Array(getMemory().buffer, ptr, len);
    const decoder = new TextDecoder();
    // Rust's formatting machinery emits one line in several writes, so a
    // console entry per write is unreadable. Held per stream until a newline.
    const pending = { 1: '', 2: '' };
    return {
      // Monotonic. The browser's coarsened resolution is reported as-is,
      // never smoothed (toolchain/custom_std/pal/time.rs states that
      // contract).
      monotonicNs: () => BigInt(Math.round(performance.now() * 1e6)),
      // Wall clock, and allowed to jump. Never used for Instant.
      wallNs: () => BigInt(Date.now()) * 1000000n,
      // crypto.getRandomValues is a CSPRNG but caps at 65536 bytes a call.
      //
      // It also REFUSES a view onto a SharedArrayBuffer — "The provided
      // ArrayBufferView value must not be shared", by spec, not by browser
      // quirk — so on a threaded build, where linear memory IS shared, filling
      // the module's memory directly throws. Same family as the `slice()` the
      // panic import needs for TextDecoder. So fill a plain scratch array and
      // copy: `TypedArray.set` onto shared memory is allowed, it is only the
      // entropy call that is fussy about where it writes.
      //
      // Unconditional rather than branched on whether this memory is shared:
      // the branch would be exercised only on the threaded configuration, and
      // the cost is one memcpy of at most 64 KiB on a path that is already
      // asking the OS for entropy.
      fillRandom: (ptr, len) => {
        for (let off = 0; off < len; off += 65536) {
          const n = Math.min(65536, len - off);
          const scratch = new Uint8Array(n);
          crypto.getRandomValues(scratch);
          bytes(ptr + off, n).set(scratch);
        }
      },
      // Whole lines only; a trailing partial line waits for its newline.
      writeText: (fd, text) => {
        pending[fd] += text;
        const lines = pending[fd].split('\n');
        pending[fd] = lines.pop();
        for (const line of lines) {
          (fd === 1 ? console.log : console.error)(line);
        }
      },
      decodeBytes: (ptr, len) => decoder.decode(bytes(ptr, len).slice()),
    };
  };

  // The `frustrate.*` imports a std built by toolchain/custom_std requires.
  // Always supplied: a module that was NOT built against a patched std simply
  // does not declare them, and WebAssembly ignores imports it was offered but
  // never asked for. So there is no configuration to get wrong here — the
  // facilities are available iff the module's std asks for them.
  const stdFacilityImports = (getMemory) => {
    const h = hostFacilities(getMemory);
    return {
      now_monotonic_ns: () => h.monotonicNs(),
      now_wall_ns: () => h.wallNs(),
      fill_random: (ptr, len) => h.fillRandom(Number(ptr), Number(len)),
      write_stdio: (stream, ptr, len) => {
        const fd = Number(stream) === 2 ? 2 : 1;
        h.writeText(fd, h.decodeBytes(Number(ptr), Number(len)));
      },
      hardware_concurrency: () =>
          (globalThis.navigator && navigator.hardwareConcurrency) || 0,
      // std::thread::sleep on the stock build, which has no wait instruction:
      // busy-wait, and only on a worker. On the main thread we throw instead;
      // the throw crosses back as a trap and $frustrateCall attributes it.
      //
      // The +atomics build never reaches this: std keeps its own futex sleep
      // there (toolchain/custom_std/tool/patch.dart, the `thread` facility).
      sleep_ns: (ns) => {
        if (typeof WorkerGlobalScope === 'undefined') {
          throw new Error('frustrate: std::thread::sleep on the main thread. ' +
              'Blocking there freezes the page. Call it on a worker — an ' +
              'Actor instance is one — or await a timer from an async fn ' +
              'instead.');
        }
        // Burns this worker's core: there is nothing to block on without
        // atomics.
        const end = performance.now() + Number(ns) / 1e6;
        while (performance.now() < end) { /* spin */ }
      },
    };
  };

  // The wasip1 shim. A bridge crate built for wasm32-wasip1 gets a std that
  // works — SystemTime, Instant, getrandom (hence rand/uuid/ahash), println!
  // — by importing `wasi_snapshot_preview1` and letting the host supply it.
  // This is that host. Defined before the main/worker branch because all
  // three instantiation sites need it: the main instance (via
  // $frustrateWasi, below), the pool worker and the actor worker.
  //
  // `getMemory` is a callback, not a value: imports are built before
  // instantiation, and the views must be rebuilt after any memory growth.
  //
  // Modules built for wasm32-unknown-unknown import none of this. Supplying
  // it anyway is free — instantiation only looks up what a module declares —
  // so there is no detection branch and no way to get the choice wrong.
  //
  // UNIMPLEMENTED CALLS THROW, they do not return an errno. That is the
  // whole safety argument: an ENOTSUP stub for `poll_oneoff` would turn
  // `thread::sleep` into a silent zero-duration no-op, which is strictly
  // worse than the loud trap it gets on wasm32-unknown-unknown today. A
  // throw crosses back as a trap and `$frustrateCall` attributes it. The
  // Proxy gives that treatment to every preview1 call we have not
  // implemented, so a dead-but-linked path in a dependency still
  // instantiates and only fails if it actually runs.
  const wasiImports = (getMemory) => {
    const view = () => new DataView(getMemory().buffer);
    const bytes = (ptr, len) => new Uint8Array(getMemory().buffer, ptr, len);
    const h = hostFacilities(getMemory);
    const unsupported = (name, detail) => () => {
      throw new Error('frustrate: the wasip1 host does not implement ' +
          name + (detail ? ' — ' + detail : '') + '.');
    };
    const impl = {
      // 0 = REALTIME (SystemTime), 1 = MONOTONIC (Instant). The CPU-time
      // clocks have no browser equivalent, so they report ENOTSUP (58)
      // rather than quietly returning a wall clock.
      clock_time_get(id, precision, out) {
        if (id !== 0 && id !== 1) return 58;
        view().setBigUint64(out, id === 0 ? h.wallNs() : h.monotonicNs(), true);
        return 0;
      },
      random_get(ptr, len) {
        h.fillRandom(ptr, len);
        return 0;
      },
      sched_yield() { return 0; },
      // stdout/stderr to the console, buffered to whole lines so a Rust
      // println! is one console entry rather than one per write.
      fd_write(fd, iovs, iovsLen, out) {
        if (fd !== 1 && fd !== 2) return 8; // EBADF
        const dv = view();
        let written = 0;
        let text = '';
        for (let i = 0; i < iovsLen; i++) {
          const p = dv.getUint32(iovs + i * 8, true);
          const n = dv.getUint32(iovs + i * 8 + 4, true);
          text += h.decodeBytes(p, n);
          written += n;
        }
        h.writeText(fd, text);
        dv.setUint32(out, written, true);
        return 0;
      },
      environ_sizes_get(countPtr, sizePtr) {
        const dv = view();
        dv.setUint32(countPtr, 0, true);
        dv.setUint32(sizePtr, 0, true);
        return 0;
      },
      environ_get() { return 0; },
      proc_exit: unsupported('proc_exit',
          'a bridge module must return, not exit the process'),
      poll_oneoff: unsupported('poll_oneoff',
          'this is usually thread::sleep, which cannot block a browser ' +
          'thread; use an async timer on the Dart side'),
    };
    return new Proxy(impl, {
      get: (target, name) => name in target
          ? target[name]
          : unsupported(String(name)),
    });
  };

  // Load the wasm-bindgen sidecar an app named in
  // `FrustrateWeb.init(bindgenGlueUrl:)`, and check it is the shape frustrate
  // can drive. Defined before the main/worker branch because all three
  // instantiation sites reach it: the main instance through
  // `$frustrateImport` below (runtime_web.dart's `_loadBindgen` is its only
  // caller), the pool worker and the actor worker through `bindgenImports`.
  //
  // `__wbg_set_wasm` is the whole check, because it is the whole handshake:
  // wasm-bindgen's `--target bundler` output is a `_bg.js` of shims closing
  // over a `let wasm` that this setter fills in, plus an entry module that
  // does nothing but call it (and `__wbindgen_start`) — which is exactly what
  // `bindgenStart` replicates, since frustrate owns instantiation. Every
  // other target instantiates the module itself and exports no setter, so a
  // namespace without one is either the wrong `--target` or not wasm-bindgen
  // output at all. Checking here rather than at the handshake is what lets
  // the message name the URL that is wrong.
  const bindgenNamespace = async (url) => {
    const ns = await import(url);
    if (typeof ns.__wbg_set_wasm !== 'function') {
      throw new Error('frustrate: ' + url + ' exports no __wbg_set_wasm, so ' +
          'it is not wasm-bindgen `--target bundler` output. Serve the ' +
          'generated *_bg.js — Bazel: the `bindgen_js` output group of ' +
          'frustrate_wasm_module(bindgen = ...).');
    }
    return ns;
  };

  if (typeof WorkerGlobalScope === 'undefined') {
    // Reachable from Dart (runtime_web.dart builds the main instance's
    // import object) and from any app that instantiates a module itself.
    globalThis.$frustrateWasi = wasiImports;
    // The main instance's import object is built in Dart
    // (runtime_web.dart); it takes the facility imports from here so
    // all three instantiation sites serve one implementation.
    globalThis.$frustrateStdFacilities = stdFacilityImports;
    // Main thread: the JS-owned stack frame for bridge calls. Rust panics
    // trap (panic=abort), and a trap is uncatchable from inside another wasm
    // module — but a JS frame CAN catch it and rethrow it as a plain JS error,
    // which the dart2wasm caller CAN catch. Remove this hop and web loses
    // BridgePanicException; //tests/dart_integration:trap_attribution_test is
    // what goes red.
    //
    // Fixed arity rather than an args array plus an `{ok|err}` verdict object:
    // under dart2wasm every `dart:js_interop` operation is a wasm→JS crossing,
    // so the calling convention, not the frame, sets the floor.
    // `tests/dart_integration/test/call_entry_count_test.dart` pins how many
    // wasm entries one `callSync` costs.
    //
    // Six parameters covers every export the runtime calls
    // (`frustrate_call_sync`, at five, is the widest); a wasm export ignores
    // arguments past its own arity, so the unused tail costs nothing and needs
    // no per-arity variants.
    //
    // The marker prefix is load-bearing: the Dart side strips back to it, so
    // a trap nothing else attributed still reports exactly the engine's own
    // message, and an error that never reached this frame (a stale glue, a
    // missing export) is recognisably NOT a Rust panic and is rethrown
    // unchanged instead of being dressed up as one.
    globalThis.$frustrateCall = function (f, a, b, c, d, e, g) {
      try {
        return f(a, b, c, d, e, g);
      } catch (err) {
        throw new Error('frustrate-wasm-trap:' + String(err));
      }
    };
    // Ran as a served <script src>: record the URL so the Dart runtime
    // creates workers from this same served file instead of blob: URLs
    // (which strict CSP blocks). The inline-injected fallback leaves it
    // unset (currentScript.src is '' there). Apps whose bundler renames
    // the asset may set $frustrateGlueUrl themselves before init.
    if (typeof document !== 'undefined' && document.currentScript &&
        document.currentScript.src) {
      globalThis.$frustrateGlueUrl = document.currentScript.src;
    }
    // The main instance's wasm-bindgen sidecar loader, for the Dart runtime.
    // A bridge crate whose graph contains wasm-bindgen (iroh, and anything
    // else that reaches the browser's own APIs from Rust) links against an
    // import namespace only wasm-bindgen's generated JS can satisfy; that
    // file is an ES module and an app asset, so it has to be loaded by URL
    // rather than embedded here. Keeping the import() in JS keeps the glue
    // the only place that ever touches script loading, and stays clean under
    // `script-src 'self'`. Async, so the check inside it reaches Dart as a
    // rejected promise rather than a synchronous throw.
    globalThis.$frustrateImport = bindgenNamespace;
    return;
  }

  // Worker: one onmessage dispatcher, two pump protocols. A given worker
  // only ever runs one of them — pool workers receive a single 'pool-init'
  // and never return from it; actor workers receive 'init' then
  // 'call'/'cancel'/'cancel-call'.
  let exports = null;
  let mem = null;
  let lastPanic = null;
  const td = new TextDecoder();

  // Satisfy every import namespace the runtime does not own itself.
  //
  // A bridge crate whose dependency graph contains wasm-bindgen links against
  // a namespace of generated shims — the module name is a relative path such
  // as './demo_bg.js', and there can be over a hundred of them. Only
  // wasm-bindgen's own generated ES module can supply them, and that module is
  // a *build artifact of the app*, not of frustrate. So it is loaded by URL,
  // exactly as the .wasm module is, and this glue file stays byte-identical
  // for every app.
  //
  // Returns the namespace object, or null if the module needs none — in which
  // case no URL was required and none is fetched. `url` is already absolute
  // (the main thread resolves it against location.href before sending, so a
  // worker's own base URL cannot change its meaning).
  //
  // `memory` is the memory THIS caller means the module to run on, and it is
  // supplied under the sidecar's namespace rather than under `env`. On a
  // threaded build wasm-bindgen's thread transform moves the memory import out
  // of `env` and into the generated module, whose top level runs
  // `export const memory = new WebAssembly.Memory({… shared: true})` — once per
  // realm, so every Worker importing it would otherwise get a *different*
  // shared memory and the threads would share nothing. Overriding the key on a
  // copy of the (frozen) namespace is enough because no shim ever reads that
  // binding: the module re-exports the memory it imports and every generated
  // view goes through `wasm.memory.buffer` on the instance. Pass null when the
  // caller has no memory to supply (single-threaded modules export their own).

  // Namespaces this runtime supplies itself, so an import from one of them is
  // NOT evidence that the module needs a wasm-bindgen sidecar. Keep in sync
  // with `_selfSuppliedNamespaces` in runtime_web.dart — same rule, two
  // contexts. `wasi_snapshot_preview1` belongs here for the same reason
  // `frustrate` does: it is in the import object a few lines below.
  const selfSupplied = ['frustrate', 'env', 'wasi_snapshot_preview1'];

  const bindgenImports = async (module, imports, url, memory) => {
    const foreign = new Set();
    for (const i of WebAssembly.Module.imports(module)) {
      if (!selfSupplied.includes(i.module)) foreign.add(i.module);
    }
    if (foreign.size === 0) return null;
    if (!url) {
      throw new Error(
          'frustrate: this bridge module imports ' + [...foreign].join(', ') +
          ', which only wasm-bindgen\'s generated JS can satisfy. Pass its ' +
          'URL as FrustrateWeb.init(bindgenGlueUrl: ...) and serve it beside ' +
          'the .wasm module.');
    }
    const ns = await bindgenNamespace(url);
    const supplied = memory ? Object.assign({}, ns, { memory }) : ns;
    for (const name of foreign) imports[name] = supplied;
    return ns;
  };

  // wasm-bindgen's two-step handshake, in the order it requires: hand the
  // generated JS the instance it closes over, THEN run the module's start
  // function. Both must precede any frustrate export — `frustrate_web_init`
  // included — because a wasm-bindgen shim called before `__wbg_set_wasm`
  // dereferences `undefined`.
  //
  // `__wbg_set_wasm` is `--target bundler`'s internal, not a documented API;
  // frustrate pins it because it owns instantiation, and this is the same
  // handshake wasm-bindgen's own generated entry module performs. It has
  // existed since 0.2.84 (before that, bundler output imported the .wasm
  // statically and no host could instantiate it at all). Its absence is
  // caught at load, in `bindgenNamespace` — nothing to re-check here.
  //
  // `__wbindgen_start` is exported iff the module had a start section, which
  // is why the call is guarded rather than assumed. On a threaded module it is
  // also the whole per-thread bootstrap (the transform unstarts the module and
  // exports its injected start), and it takes one argument: the stack size in
  // bytes for THIS thread, ignored on the first thread of a memory because
  // that thread keeps the linked stack pointer. 0 — which is what an omitted
  // `stackSize` becomes on the way into wasm — means the transform's own 2 MiB
  // default. Only the pool pump passes one; every other caller is a first
  // thread, where the value cannot matter.
  const bindgenStart = (ns, instanceExports, stackSize) => {
    if (!ns) return;
    ns.__wbg_set_wasm(instanceExports);
    if (instanceExports.__wbindgen_start) {
      instanceExports.__wbindgen_start(stackSize);
    }
  };

  // `-fstack-protector` is hardcoded in toolchains_llvm's default compile
  // flags and is not behind a Bazel feature, so any C in the crate graph
  // (ring, reached through rustls) leaves exactly one unremovable import.
  // Supplying it is correct rather than a workaround: if it is ever actually
  // called the stack really is smashed, and throwing turns that into a trap
  // the call frame reports instead of undefined behaviour.
  const stackChkFail = () => {
    throw new Error('frustrate: stack smashing detected in the bridge module');
  };

  // Pool-worker pump (threaded wasm): instantiate the shared module on the
  // SHARED memory, get this thread a stack and a TLS block, then enter the
  // runtime's worker loop (frustrate_worker_entry), which only returns by
  // trapping. Completions relay (ptr, len) to the main thread — zero
  // copies; the memory is shared and the main thread frees the buffer
  // after reading.
  //
  // The memory arrives twice, under two namespaces, because which one the
  // module asks through depends on whether the wasm-bindgen post-pass ran:
  // `env.memory` is where rustc's own output imports it, the sidecar namespace
  // is where the thread transform moves it to (see `bindgenImports`). Both are
  // supplied unconditionally — instantiation only looks up what a module
  // declares — so there is one shared memory either way and no branch to get
  // wrong.
  const poolInit = async (m) => {
    const { module, memory, stackTop, tlsPtr, entryPtr, stackSize } = m;
    let exports = null;
    // Whether this worker ever entered the runtime's loop, which is what
    // separates the two failure reports below. Instantiating is not the line:
    // the sidecar handshake, the stack pointer and TLS init all run on a live
    // `exports` and still precede any job.
    let entered = false;
    const imports = {
      env: { memory, __stack_chk_fail: stackChkFail },
      // The pool worker shares the module's memory, so the shim reads the
      // same linear memory the main thread does.
      wasi_snapshot_preview1: wasiImports(() => memory),
      frustrate: {
        ...stdFacilityImports(() => memory),
        post(callId, ptr, len, cap) {
          postMessage({ type: 'post', callId: Number(callId), ptr: Number(ptr),
                        len: Number(len), cap: Number(cap) });
        },
        panic(ptr, len) {
          // slice(): TextDecoder refuses SharedArrayBuffer-backed views.
          lastPanic = td.decode(
              new Uint8Array(memory.buffer, Number(ptr), Number(len)).slice());
        },
        spawn_worker() {
          throw new Error('frustrate: pool workers must not spawn workers');
        },
        // Declared by every module that links the cooperative executor, and
        // never called from a pool worker: on threaded wasm the Scheduler
        // hands ready tasks to the pool itself, and only an actor instance
        // takes the microtask path (executor.rs, `arrange_drain`). Supplied
        // anyway because instantiation resolves every import a module
        // *declares*, reachable or not — without it a bridge with any async
        // fn in it fails to link here, and the whole threaded pool dies at
        // startup with `LinkError: "schedule_drain": function import requires
        // a callable`. That is how this was found.
        //
        // Throwing rather than no-op'ing, like the wasip1 shim above and for
        // the same reason: this worker's entry loop blocks its thread, so a
        // drain arranged here could never run, and a silently dropped poll is
        // a Dart future that hangs forever.
        schedule_drain() {
          throw new Error('frustrate: a pool worker arranged an executor ' +
              'drain, which its blocking entry loop can never run — the ' +
              'threaded Scheduler must hand ready tasks to the pool');
        },
      },
    };
    try {
      const ns = await bindgenImports(module, imports, m.bindgenGlueUrl, memory);
      const instance = await WebAssembly.instantiate(module, imports);
      exports = instance.exports;
      bindgenStart(ns, exports, stackSize);
      // Two bootstraps, and the module says which it wants. wasm-bindgen's
      // thread transform DELETES `__wasm_init_tls`/`__tls_size`/`__tls_align`
      // and takes per-thread setup over itself, inside the `__wbindgen_start`
      // that `bindgenStart` just called — so on a post-passed module this
      // thread already has its stack and its TLS block, and the page sent
      // neither pointer. Keyed on the export rather than on the message so an
      // older served glue meeting a newer module fails here, loudly, instead of
      // running a half-bootstrapped thread.
      if (exports.__wasm_init_tls) {
        exports.__stack_pointer.value = stackTop;
        exports.__wasm_init_tls(tlsPtr);
      }
      entered = true;
      exports.frustrate_worker_entry(entryPtr);
      throw new Error('frustrate: pool worker loop returned');
    } catch (err) {
      let callId = 0;
      if (entered) {
        // Attribute the trap to exactly one future. A plain (non-async-fn) job
        // records its id in frustrate_current_call; an async-fn body polled by
        // the cooperative executor records it in frustrate_current_drain_call.
        // At most one is nonzero (each clears to 0 after its job), so a trap in
        // either job kind names the right call.
        try {
          callId = Number(exports.frustrate_current_call())
                || Number(exports.frustrate_current_drain_call());
        } catch (_) {}
      }
      // Which report this is decides what the page does with the worker, so
      // the line matters: `trap` means "this worker ran a job and died", and
      // the page retires it and immediately spawns a replacement. A failure
      // that happened before the loop was entered would fail the replacement
      // the same way, forever — `initError` is the branch built for that, and
      // it latches the pool as degraded or fatal instead of respawning.
      if (entered) {
        postMessage({ type: 'trap', callId, message: lastPanic || String(err) });
      } else {
        postMessage({ type: 'initError', message: lastPanic || String(err) });
      }
      lastPanic = null;
    }
  };

  // Actor pump: instantiates its own instance of the shared module and
  // drives the same frustrate_call_async export the main transport uses
  // (inline execution, serialized by the instance's single thread).
  // Channel = postMessage with transferred ArrayBuffers. This frame catches
  // traps directly — no $frustrateCall needed off the main thread.
  onmessage = async (e) => {
    const m = e.data;
    if (m.type === 'pool-init') {
      await poolInit(m);
    } else if (m.type === 'init') {
      // `mem` is assigned after instantiation below, which is why the shim
      // takes a getter rather than a memory.
      const imports = { wasi_snapshot_preview1: wasiImports(() => mem),
        frustrate: {
        ...stdFacilityImports(() => mem),
        post(callId, ptr, len, cap) {
          const view = new Uint8Array(mem.buffer, Number(ptr), Number(len));
          const copy = new Uint8Array(view);
          exports.frustrate_buffer_free(ptr, len, cap);
          postMessage({ type: 'resp', callId: Number(callId), buf: copy.buffer }, [copy.buffer]);
        },
        panic(ptr, len) {
          // slice(): TextDecoder refuses SharedArrayBuffer-backed views.
          lastPanic = td.decode(
              new Uint8Array(mem.buffer, Number(ptr), Number(len)).slice());
        },
        // The cooperative executor's Scheduler hook, draining on a
        // microtask like the main thread. Live, not vestigial: a deferred
        // actor method (Deferred<T>) spawns its completion on this
        // instance's executor, and this is what polls it. A trap during the
        // drain (panic=abort) must be attributed, not swallowed: the
        // executor records the polled call in
        // frustrate_current_drain_call, and the pump posts the same trap
        // shape the dispatch path does — otherwise a panicking deferred
        // future would be a silently hung Dart future.
        schedule_drain() {
          queueMicrotask(() => {
            try { exports.frustrate_drain(); } catch (err) {
              let callId = 0;
              try { callId = Number(exports.frustrate_current_drain_call()); } catch (_) {}
              postMessage({ type: 'trap', callId, message: lastPanic || String(err) });
              lastPanic = null;
            }
          });
        },
        // Unconditional, and that is the fix rather than the tidy-up: an
        // actor's inline-dispatched methods never touch the async pool, so a
        // spawn_worker call from inside one is a bridge bug — but a threaded
        // module DECLARES the import whether or not it can reach it, and
        // instantiation resolves every import a module declares. Supplying it
        // only when some other fact about the module held is how this came to
        // depend on where the memory import lived, which the wasm-bindgen
        // thread transform moves (LinkError: "spawn_worker": function import
        // requires a callable).
        spawn_worker() {
          throw new Error(
              'frustrate: actor instances must not spawn pool workers');
        },
      } };
      // Threaded wasm modules import their memory; single-threaded ones export
      // one. The actor's executor is still one instance with instance-local
      // state — it just allocates its own (shared-with-nobody) memory here
      // instead of exporting one, and hands it to every namespace the module
      // might ask through. Detected by import KIND, not by name: rustc puts the
      // memory in `env`, wasm-bindgen's thread transform moves it into the
      // sidecar's namespace, and only "imports a memory at all" is true of
      // both.
      let created = null;
      imports.env = { __stack_chk_fail: stackChkFail };
      if (WebAssembly.Module.imports(m.module).some(
          (i) => i.kind === 'memory')) {
        created = new WebAssembly.Memory(
            { initial: m.memInitial, maximum: m.memMaximum, shared: true });
        imports.env.memory = created;
      }
      try {
        const ns =
            await bindgenImports(m.module, imports, m.bindgenGlueUrl, created);
        const instance = await WebAssembly.instantiate(m.module, imports);
        exports = instance.exports;
        mem = created || exports.memory;
        bindgenStart(ns, exports);
        exports.frustrate_web_init();
        // Tell the executor this instance is an actor's: on threaded builds
        // its Scheduler must use the microtask drain above rather than the
        // pool it does not have (a deferred method is what spawns on it).
        exports.frustrate_mark_actor_instance();
        postMessage({ type: 'ready' });
      } catch (err) {
        postMessage({ type: 'initError', message: lastPanic || String(err) });
      }
    } else if (m.type === 'call') {
      // The whole buffer is the request. The sender allocates it at exactly
      // the request's size and transfers it, so there is no unused capacity
      // to exclude — which there was when the buffer was a view over the Dart
      // writer's own storage, as wide as that writer ever grew.
      const req = new Uint8Array(m.buf);
      let ptr = 0;
      if (req.length > 0) {
        ptr = exports.frustrate_alloc(BigInt(req.length));
        new Uint8Array(mem.buffer, Number(ptr), req.length).set(req);
      }
      try {
        exports.frustrate_call_async(m.fnId, ptr, BigInt(req.length), BigInt(m.callId));
      } catch (err) {
        postMessage({ type: 'trap', callId: m.callId, message: lastPanic || String(err) });
        lastPanic = null;
      } finally {
        if (req.length > 0) {
          exports.frustrate_buffer_free(ptr, BigInt(req.length), BigInt(req.length));
        }
      }
    } else if (m.type === 'cancel-call') {
      // A FrustrateCancelToken reaching a deferred completion parked on THIS
      // instance's cooperative executor. Only this worker can run the claim:
      // the task and the future are in this instance's linear memory, which
      // the page cannot touch.
      //
      // The reply is the other half of the claim protocol, and the Dart side
      // settles the future off it. Claimed (1) means no response for this call
      // will ever be posted; not claimed (0) means the completion already
      // happened, and postMessage's FIFO guarantees its 'resp' left ahead of
      // this reply, so the future is already settled over there.
      //
      // Ordering is why the claim can be trusted: a 'cancel-call' queues behind
      // the 'call' that created the task, so the prefix has always run by now.
      //
      // The future itself is dropped on the next microtask drain, not here —
      // frustrate_call_cancel only claims the registry entry.
      //
      // Version skew, since a served glue can be older than the runtime that
      // posts to it: a glue without this arm ignores the message, so the
      // cancel simply does not take and the call keeps its real answer — the
      // same compatibility class as pause/resume. Worth
      // knowing that it reads differently, though: a deferred completion that
      // never resolves then waits for dispose() rather than erroring.
      let claimed = false;
      try {
        claimed = exports.frustrate_call_cancel(BigInt(m.callId)) !== 0;
      } catch (err) {
        postMessage({ type: 'trap', callId: m.callId, message: lastPanic || String(err) });
        lastPanic = null;
      }
      postMessage({ type: 'cancelled', callId: m.callId, claimed });
    } else if (m.type === 'cancel') {
      // Cooperative stream cancel for this instance's registry. Delivered
      // when the executor is idle (this event loop) — a producer that never
      // returns from its method never observes it; worker-instance physics.
      exports.frustrate_stream_cancel(BigInt(m.streamId));
    } else if (m.type === 'pause') {
      // Backpressure for this instance's registry (StreamController.onPause).
      // Same idle-delivery physics as cancel: an async producer with .await
      // points parks on the next send(); a never-yielding one does not.
      exports.frustrate_stream_pause(BigInt(m.streamId));
    } else if (m.type === 'resume') {
      exports.frustrate_stream_resume(BigInt(m.streamId));
    }
  };
})();
''';
