// Threaded wasm spike, browser side. Sequence:
//   1. instantiate the +atomics module on a shared WebAssembly.Memory
//   2. start_counters(4, 100000): shared atomics + std Mutex across threads
//   3. speedup: work_serial(4, n) on main vs start_work(4, n) across workers
// Results go to the console as `RESULTS {json}` (Playwright scrapes them).

const out = (s) => {
  document.getElementById('out').textContent = s;
};

async function run() {
  const results = {};

  const memory = new WebAssembly.Memory({
    initial: 320,
    maximum: 4096,
    shared: true,
  });
  const module = await WebAssembly.compileStreaming(fetch('module.wasm'));

  let instance;
  const workers = [];
  function spawnWorker(entryPtr) {
    const stackSize = 1 << 20;
    const stackPtr = instance.exports.wasm_alloc(stackSize, 16);
    const tlsSize = instance.exports.__tls_size.value;
    if (tlsSize <= 0) throw new Error(`expected nonzero TLS size: ${tlsSize}`);
    const tlsPtr = instance.exports.wasm_alloc(
      tlsSize,
      instance.exports.__tls_align.value,
    );
    const w = new Worker('worker.js');
    w.onmessage = (e) => {
      if (!e.data.ok) {
        console.log(`SPIKE_ERROR worker: ${e.data.error}`);
      }
    };
    w.postMessage({
      module,
      memory,
      stackTop: stackPtr + stackSize,
      tlsPtr,
      entryPtr,
    });
    workers.push(w);
  }

  instance = await WebAssembly.instantiate(module, {
    env: { memory },
    frustrate: { spawn_worker: spawnWorker },
  });
  results.instantiated = true;
  results.tlsSize = instance.exports.__tls_size.value;

  const waitDone = async (n) => {
    while (instance.exports.done_count() < n) {
      await new Promise((r) => setTimeout(r, 5));
    }
  };

  // Correctness across threads: atomics + futex-backed Mutex.
  instance.exports.start_counters(4, 100000);
  await waitDone(4);
  results.counter = instance.exports.counter();
  results.lockedSum = Number(instance.exports.locked_sum());

  // Speedup, including cold spawn overhead (the pool pre-spawns; this
  // measures the worst case).
  const n = 20000;
  let t0 = performance.now();
  instance.exports.work_serial(4, n);
  results.serialMs = performance.now() - t0;

  instance.exports.reset_done();
  t0 = performance.now();
  instance.exports.start_work(4, n);
  await waitDone(4);
  results.parallelMs = performance.now() - t0;
  results.speedup = results.serialMs / results.parallelMs;

  for (const w of workers) w.terminate();
  console.log(`RESULTS ${JSON.stringify(results)}`);
  out(JSON.stringify(results, null, 2));
}

run().catch((e) => {
  console.log(`SPIKE_ERROR ${e}\n${e.stack}`);
  out(`failed: ${e}`);
});
