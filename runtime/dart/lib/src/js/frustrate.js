// frustrate's web glue: the one JavaScript file of the runtime. Served as a
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
  // and the `frustrate.*` browser-std imports (where the browser std asks us
  // directly). One implementation, so the two branches
  // cannot answer a question differently.
  const hostAnswers = (getMemory) => {
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
  // browser std's calls are served iff the module's std asks for them.
  const browserStdImports = (getMemory) => {
    const h = hostAnswers(getMemory);
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
    const h = hostAnswers(getMemory);
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

  // Namespaces the runtime puts in every import object itself. Every other
  // namespace a module imports is supplied by a host module: an ES module the
  // app serves, named by URL in `FrustrateWeb.init`. //bazel/wasm_import_check
  // admits the same three at build time; host_imports_web_test.dart runs this
  // file against that tool's list.
  const selfSupplied = ['frustrate', 'env', 'wasi_snapshot_preview1'];

  // Whether `ns` is wasm-bindgen's namespace. `--target bundler` writes the one
  // namespace its generated JS supplies as `./<out-name>_bg.js`, so a namespace
  // of that form is the glue's (`bindgenGlueUrl`) and no other is.
  // //bazel/wasm_import_check holds a build to the same rule.
  const isBindgenNamespace = (ns) =>
      ns.startsWith('./') && ns.endsWith('_bg.js');

  // A host module the app wrote: its exports are the namespace's functions,
  // and its optional `attach({exports, memory, actor})` is its attach step.
  const appHost = async (ns, url) => {
    let host;
    try {
      host = await import(url);
    } catch (err) {
      throw new Error('frustrate: loading the module for import namespace \'' +
          ns + '\' from ' + url + ', named in ' +
          'FrustrateWeb.init(hostImports:), failed: ' + String(err));
    }
    return {
      exports: host,
      attach: ({ exports, memory, actor }) => typeof host.attach === 'function'
          ? host.attach({ exports, memory, actor })
          : undefined,
    };
  };

  // wasm-bindgen's generated JS (`--target bundler`'s `_bg.js`) as a host
  // module. Everything wasm-bindgen-specific about loading a module is here.
  //
  // Its attach step is wasm-bindgen's two-step handshake, in the order it
  // requires: hand the generated JS the instance it closes over, THEN run the
  // module's start function. `__wbg_set_wasm` is `--target bundler`'s
  // internal, not a documented API; frustrate pins it because it owns
  // instantiation, and this is the same handshake wasm-bindgen's own generated
  // entry module performs. It has existed since 0.2.84. Every other target
  // instantiates the module itself and exports no setter, so a module without
  // one is the wrong `--target` or not wasm-bindgen output at all — checked at
  // load, where the message can name the URL.
  //
  // `__wbindgen_start` is exported iff the module had a start section, which
  // is why the call is guarded. On a threaded module it is also the whole
  // per-thread bootstrap (the transform unstarts the module and exports its
  // injected start), and it takes the stack size in bytes for THIS thread,
  // ignored on the first thread of a memory because that thread keeps the
  // linked stack pointer. 0 — what an omitted `stackSize` becomes on the way
  // into wasm — means the transform's own 2 MiB default. Only the pool pump
  // passes one; every other caller is a first thread.
  //
  // Two waits are reachable from `__wbindgen_start` on the main thread: the
  // transform's temporary-stack lock and lld's passive-data-init barrier.
  // Neither can contend, because the main thread's `__wbindgen_start` finishes
  // before any other thread exists on its memory: pool workers are spawned
  // only from inside a bridge call, and each actor is thread 0 of a memory it
  // created. Keep it that way, and never call `__wbindgen_thread_destroy` from
  // the main thread.
  //
  // `memory` is the memory THIS caller means the module to run on, supplied in
  // place of the module's own export of that name. On a threaded build the
  // thread transform moves the memory import out of `env` and into this
  // namespace, and the generated JS's top level runs
  // `export const memory = new WebAssembly.Memory({… shared: true})` — once per
  // realm, so every Worker importing it would otherwise get a *different*
  // shared memory and the threads would share nothing. Overriding the key on a
  // copy of the (frozen) namespace is enough because no shim ever reads that
  // binding: the module re-exports the memory it imports and every generated
  // view goes through `wasm.memory.buffer` on the instance. Null when the
  // caller has no memory to supply (single-threaded modules export their own).
  const bindgenHost = async (url, memory) => {
    const ns = await import(url);
    if (typeof ns.__wbg_set_wasm !== 'function') {
      throw new Error('frustrate: ' + url + ' exports no __wbg_set_wasm, so ' +
          'it is not wasm-bindgen `--target bundler` output. Serve the ' +
          'generated *_bg.js — Bazel: the `bindgen_js` output group of ' +
          'frustrate_wasm_module(bindgen = ...).');
    }
    return {
      exports: memory ? Object.assign({}, ns, { memory }) : ns,
      attach: ({ exports, stackSize }) => {
        ns.__wbg_set_wasm(exports);
        if (exports.__wbindgen_start) exports.__wbindgen_start(stackSize);
      },
    };
  };

  // Supply every import namespace the runtime does not own itself, from the
  // host modules the app named in `FrustrateWeb.init`, and answer the attach
  // step: `attach({exports, memory, actor, stackSize})` runs each host's after
  // instantiation, awaiting each in turn, and the caller runs it before any
  // frustrate export. Defined before the main/worker branch because all three
  // instantiation sites call it: the main instance (through
  // `$frustrateHostImports`, from runtime_web.dart), the pool worker and the
  // actor worker. Each realm imports its own copy of every module, so no
  // module state is shared between realms.
  //
  // `hostUrls` maps a namespace to its module's URL. `bindgenUrl` is
  // wasm-bindgen's generated JS, which supplies the module's wasm-bindgen
  // namespace (`isBindgenNamespace`). URLs are already absolute: the page
  // resolves them against its own location before any worker sees them.
  //
  // `actor` is `{bridge, type}` for the actor the instance serves, and null on
  // the page and in pool workers. The wasm-bindgen glue attaches first,
  // because a shim called before its handshake dereferences `undefined`; the
  // app's hosts follow in namespace order.
  const hostImports = async (module, imports, hostUrls, bindgenUrl, memory) => {
    const urls = hostUrls || {};
    for (const ns of Object.keys(urls)) {
      if (selfSupplied.includes(ns)) {
        throw new Error('frustrate: FrustrateWeb.init(hostImports:) names ' +
            'the namespace \'' + ns + '\', which the runtime supplies itself.');
      }
      if (isBindgenNamespace(ns)) {
        throw new Error('frustrate: FrustrateWeb.init(hostImports:) names ' +
            'the namespace \'' + ns + '\', which is wasm-bindgen\'s. Name ' +
            'its generated JS in FrustrateWeb.init(bindgenGlueUrl:) instead.');
      }
    }
    const foreign = [...new Set(WebAssembly.Module.imports(module)
        .map((i) => i.module)
        .filter((ns) => !selfSupplied.includes(ns)))].sort();
    const named = foreign.filter((ns) => urls[ns]);
    const bindgen = foreign.filter(isBindgenNamespace);
    const unnamed = foreign.filter((ns) => !urls[ns] && !isBindgenNamespace(ns));
    if (unnamed.length > 0) {
      const quoted = unnamed.map((ns) => '\'' + ns + '\'');
      const one = unnamed.length === 1;
      throw new Error('frustrate: this bridge module imports the namespace' +
          (one ? ' ' : 's ') + quoted.join(', ') +
          ', and FrustrateWeb.init(hostImports:) names no module for ' +
          (one ? 'it' : 'them') + '. Pass hostImports: {' +
          quoted.map((ns) => ns + ': \'<url>\'').join(', ') +
          '}, naming the ES module that implements each, and serve it beside ' +
          'the .wasm module.');
    }
    if (bindgen.length > 0 && !bindgenUrl) {
      throw new Error('frustrate: this bridge module imports wasm-bindgen\'s ' +
          'namespace \'' + bindgen[0] + '\'. Serve its generated JS beside ' +
          'the .wasm module and name it in ' +
          'FrustrateWeb.init(bindgenGlueUrl: ...).');
    }
    const glue = bindgen.length > 0
        ? await bindgenHost(bindgenUrl, memory)
        : null;
    const hosts = await Promise.all(named.map((ns) => appHost(ns, urls[ns])));
    named.forEach((ns, i) => { imports[ns] = hosts[i].exports; });
    for (const ns of bindgen) imports[ns] = glue.exports;
    const ordered = glue ? [glue, ...hosts] : hosts;
    return {
      attach: async (context) => {
        for (const host of ordered) await host.attach(context);
      },
    };
  };

  if (typeof WorkerGlobalScope === 'undefined') {
    // Reachable from Dart (runtime_web.dart builds the main instance's
    // import object) and from any app that instantiates a module itself.
    globalThis.$frustrateWasi = wasiImports;
    // The main instance's import object is built in Dart
    // (runtime_web.dart); it takes the browser-std imports from here so
    // all three instantiation sites serve one implementation.
    globalThis.$frustrateBrowserStd = browserStdImports;
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
    // (a bridge's `call_sync`, at five, is the widest); a wasm export
    // ignores arguments past its own arity, so the unused tail costs nothing
    // and needs no per-arity variants.
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
    // The main instance's loader for the import namespaces apps supply, for
    // the Dart runtime. Keeping the import() in JS keeps the glue the only
    // place that ever touches script loading, and stays clean under
    // `script-src 'self'`. Async, so its checks reach Dart as a rejected
    // promise rather than a synchronous throw.
    globalThis.$frustrateHostImports = hostImports;
    return;
  }

  // Worker: one onmessage dispatcher, two pump protocols. A given worker
  // only ever runs one of them — pool workers receive a single 'pool-init'
  // and never return from it; actor workers receive 'init' then
  // 'call'/'cancel'/'cancel-call'.
  let exports = null;
  let mem = null;
  let lastPanic = null;
  // The actor's bridge's `call_async` export, named by the init message: one
  // instance serves one bridge, and the bridge's exports carry its crate name.
  let callAsync = null;
  const td = new TextDecoder();

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
  // `env.memory` is where rustc's own output imports it, the wasm-bindgen
  // glue's namespace is where the thread transform moves it to (see
  // `bindgenHost`). Both are
  // supplied unconditionally — instantiation only looks up what a module
  // declares — so there is one shared memory either way and no branch to get
  // wrong.
  const poolInit = async (m) => {
    const { module, memory, stackTop, tlsPtr, entryPtr, stackSize } = m;
    let exports = null;
    // Whether this worker ever entered the runtime's loop, which is what
    // separates the two failure reports below. Instantiating is not the line:
    // the stack pointer and TLS init and the attach step all run on a live
    // `exports` and still precede any job.
    let entered = false;
    const imports = {
      env: { memory, __stack_chk_fail: stackChkFail },
      // The pool worker shares the module's memory, so the shim reads the
      // same linear memory the main thread does.
      wasi_snapshot_preview1: wasiImports(() => memory),
      frustrate: {
        ...browserStdImports(() => memory),
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
        // Declared by a module whose actor takes an `Addr`, and reachable
        // only from an actor instance: `Addr` is made only on an actor's
        // executor. Supplied, throwing, for schedule_drain's reason.
        self_post() {
          throw new Error('frustrate: Addr::post reached a pool worker, ' +
              'which hosts no actor — bridge bug');
        },
      },
    };
    try {
      const hosts = await hostImports(
          module, imports, m.hostImports, m.bindgenGlueUrl, memory);
      const instance = await WebAssembly.instantiate(module, imports);
      exports = instance.exports;
      // Two bootstraps, and the module says which it wants. wasm-bindgen's
      // thread transform DELETES `__wasm_init_tls`/`__tls_size`/`__tls_align`
      // and takes per-thread setup over itself, inside the `__wbindgen_start`
      // that its glue's attach step calls below — so on a post-passed module
      // the page sent neither pointer. Keyed on the export rather than on the
      // message so an older served glue meeting a newer module fails here,
      // loudly, instead of running a half-bootstrapped thread. Before the
      // attach step, so a host's attach may call into the module.
      if (exports.__wasm_init_tls) {
        exports.__stack_pointer.value = stackTop;
        exports.__wasm_init_tls(tlsPtr);
      }
      // A pool worker serves no actor.
      await hosts.attach({ exports, memory, actor: null, stackSize });
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

  // An actor's posts to itself (`Addr::post`). Each post sends one token
  // through this worker-local channel, and each token, delivered as a task of
  // its own, runs the oldest closure the instance has waiting. A task, not a
  // microtask, so the event loop dispatches whatever else is ready (the
  // page's calls) between two slices of a job that keeps posting; a
  // MessageChannel rather than setTimeout(0), which browsers clamp to 4 ms
  // once timers nest, an idle gap after every slice. A trap here belongs to
  // no call, so it is reported under call id 0, which the page hands to the
  // actor's zone. If an `Addr::call` waits on the closure,
  // `frustrate_posted_trapped` resolves it to `Err(Gone)`; it touches only
  // that call's state. Pool workers host no actor and never post to it.
  const posted = new MessageChannel();
  posted.port1.onmessage = () => {
    try {
      exports.frustrate_run_posted();
    } catch (err) {
      postMessage({ type: 'trap', callId: 0, message: lastPanic || String(err) });
      lastPanic = null;
      try {
        exports.frustrate_posted_trapped();
      } catch (again) {
        postMessage({ type: 'trap', callId: 0, message: lastPanic || String(again) });
        lastPanic = null;
      }
    }
  };

  // Errors that escape outside every entry point above, once the actor is up:
  // a Rust panic inside a wasm-bindgen closure the browser invoked (an event
  // handler, a timer, a promise reaction) and a rejected promise nobody
  // handled. No call owns either, so the page hands each to the actor's zone:
  // a panic (or any other error) as a trap under call id 0, and a rejection
  // that no panic caused as a `rejection`. preventDefault keeps it from also
  // reaching the page's `Worker.onerror` and the console.
  const watchEscapes = () => {
    addEventListener('error', (e) => {
      e.preventDefault();
      postMessage({ type: 'trap', callId: 0,
                    message: lastPanic || String(e.error ?? e.message) });
      lastPanic = null;
    });
    addEventListener('unhandledrejection', (e) => {
      e.preventDefault();
      postMessage(lastPanic
          ? { type: 'trap', callId: 0, message: lastPanic }
          : { type: 'rejection', message: String(e.reason) });
      lastPanic = null;
    });
  };

  // Actor pump: instantiates its own instance of the shared module and
  // drives the same call_async export the main transport uses
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
        ...browserStdImports(() => mem),
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
        // `Addr::post` (runtime/rust/src/addr.rs): the closure waits in this
        // instance, and this schedules a task of this worker's own to run
        // it. See `posted` above.
        self_post() {
          posted.port2.postMessage(null);
        },
      } };
      // Threaded wasm modules import their memory; single-threaded ones export
      // one. The actor's executor is still one instance with instance-local
      // state — it just allocates its own (shared-with-nobody) memory here
      // instead of exporting one, and hands it to every namespace the module
      // might ask through. Detected by import KIND, not by name: rustc puts the
      // memory in `env`, wasm-bindgen's thread transform moves it into its
      // glue's namespace, and only "imports a memory at all" is true of
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
        const hosts = await hostImports(
            m.module, imports, m.hostImports, m.bindgenGlueUrl, created);
        const instance = await WebAssembly.instantiate(m.module, imports);
        exports = instance.exports;
        callAsync = exports[m.callAsync];
        mem = created || exports.memory;
        // The page sends no call before 'ready', so nothing reaches the
        // instance while this is pending.
        await hosts.attach({ exports, memory: mem, actor: m.actor });
        exports.frustrate_web_init();
        // Tell the executor this instance is an actor's: on threaded builds
        // its Scheduler must use the microtask drain above rather than the
        // pool it does not have (a deferred method is what spawns on it).
        exports.frustrate_mark_actor_instance();
        watchEscapes();
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
        callAsync(m.fnId, ptr, BigInt(req.length), BigInt(m.callId));
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
