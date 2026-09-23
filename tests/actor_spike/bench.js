// Actor channel micro-benchmark: SAB ring vs postMessage (copy and
// transfer), round-trip through echo workers, across payload sizes.
import { Ring, ringLayout } from './ring.js';

const CAPACITY = 256 * 1024;
const SIZES = [
  { label: '8B', bytes: 8, iters: 2000 },
  { label: '1KiB', bytes: 1024, iters: 2000 },
  { label: '64KiB', bytes: 64 * 1024, iters: 500 },
  { label: '1MiB', bytes: 1024 * 1024, iters: 100 },
  { label: '8MiB', bytes: 8 * 1024 * 1024, iters: 20 },
];

function stats(samplesUs) {
  const s = [...samplesUs].sort((a, b) => a - b);
  const pick = (q) => s[Math.min(s.length - 1, Math.floor(q * s.length))];
  return {
    median_us: +pick(0.5).toFixed(1),
    p10_us: +pick(0.1).toFixed(1),
    p90_us: +pick(0.9).toFixed(1),
    n: s.length,
  };
}

async function setupRingWorker() {
  const sab = new SharedArrayBuffer(2 * ringLayout(CAPACITY));
  const req = new Ring(sab, 0, CAPACITY);
  const resp = new Ring(sab, ringLayout(CAPACITY), CAPACITY);
  const worker = new Worker('./ring_worker.js', { type: 'module' });
  await new Promise((resolve) => {
    worker.onmessage = resolve;
    worker.postMessage({
      sab,
      capacity: CAPACITY,
      reqOffset: 0,
      respOffset: ringLayout(CAPACITY),
    });
  });
  return { worker, req, resp };
}

async function setupPmWorker() {
  const worker = new Worker('./pm_worker.js');
  await new Promise((resolve) => {
    worker.onmessage = resolve;
    worker.postMessage('init');
  });
  return worker;
}

async function benchRing(req, resp, bytes, iters) {
  const payload = new Uint8Array(bytes).fill(7);
  const samples = [];
  for (let i = 0; i < iters; i++) {
    const t0 = performance.now();
    // Concurrent: streaming sends larger than the ring deadlock otherwise
    // (the echo starts coming back before the request finishes going out).
    const [, echoed] = await Promise.all([
      req.writeFrame(payload),
      resp.readFrame(),
    ]);
    samples.push((performance.now() - t0) * 1000);
    if (echoed.length !== bytes) throw new Error('ring echo length mismatch');
  }
  return stats(samples);
}

function benchPm(worker, bytes, iters, transfer) {
  return new Promise((resolve, reject) => {
    let buf = new ArrayBuffer(bytes);
    new Uint8Array(buf).fill(7);
    const samples = [];
    let i = 0;
    let t0;
    worker.onmessage = (e) => {
      samples.push((performance.now() - t0) * 1000);
      if (e.data.buf.byteLength !== bytes) {
        reject(new Error('pm echo length mismatch'));
        return;
      }
      buf = e.data.buf; // transferred back (or a fresh clone) — reusable
      if (++i >= iters) {
        resolve(stats(samples));
        return;
      }
      send();
    };
    function send() {
      t0 = performance.now();
      worker.postMessage({ buf, transfer }, transfer ? [buf] : []);
    }
    send();
  });
}

async function main() {
  const results = { capacity: CAPACITY, scenarios: [] };
  const { worker: ringWorker, req, resp } = await setupRingWorker();
  const pmWorker = await setupPmWorker();

  for (const { label, bytes, iters } of SIZES) {
    const ring = await benchRing(req, resp, bytes, iters);
    const pmCopy = await benchPm(pmWorker, bytes, iters, false);
    const pmTransfer = await benchPm(pmWorker, bytes, iters, true);
    results.scenarios.push({ size: label, bytes, ring, pmCopy, pmTransfer });
  }

  req.writeFrame(new Uint8Array([0xff])); // shutdown
  ringWorker.terminate();
  pmWorker.terminate();

  document.getElementById('out').textContent = JSON.stringify(
    results,
    null,
    2,
  );
  console.log('RESULTS ' + JSON.stringify(results));
}

main().catch((e) => {
  console.log('BENCH_ERROR ' + (e.stack || e));
  document.getElementById('out').textContent = String(e);
});
