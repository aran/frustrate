// SPSC byte ring over a SharedArrayBuffer — the candidate Actor channel.
//
// Layout (per direction): Int32 ctrl[2] = {head, tail} followed by data
// bytes. head = total bytes ever written, tail = total bytes ever read
// (free-running u32 counters; index = counter % capacity). Frames are
// [len: u32 LE][payload]; frames larger than the ring stream through it —
// chunking is just ring streaming with backpressure, not a special mode.
//
// Producer waits on tail movement when full; consumer waits on head
// movement when empty. Workers block (Atomics.wait); the main thread must
// not, so it gets async variants (Atomics.waitAsync).

const CTRL_INTS = 2;
const HEAD = 0;
const TAIL = 1;

export function ringLayout(capacity) {
  return CTRL_INTS * 4 + capacity;
}

export class Ring {
  constructor(sab, byteOffset, capacity) {
    this.ctrl = new Int32Array(sab, byteOffset, CTRL_INTS);
    this.data = new Uint8Array(sab, byteOffset + CTRL_INTS * 4, capacity);
    this.cap = capacity;
  }

  used() {
    return (
      (Atomics.load(this.ctrl, HEAD) - Atomics.load(this.ctrl, TAIL)) >>> 0
    );
  }

  free() {
    return this.cap - this.used();
  }

  // ---- raw copy (no waiting; caller guarantees space/data) ----

  copyIn(bytes, from, n) {
    const head = Atomics.load(this.ctrl, HEAD) >>> 0;
    const idx = head % this.cap;
    const first = Math.min(n, this.cap - idx);
    this.data.set(bytes.subarray(from, from + first), idx);
    if (n > first) this.data.set(bytes.subarray(from + first, from + n), 0);
    Atomics.store(this.ctrl, HEAD, (head + n) | 0);
    Atomics.notify(this.ctrl, HEAD);
  }

  copyOut(out, at, n) {
    const tail = Atomics.load(this.ctrl, TAIL) >>> 0;
    const idx = tail % this.cap;
    const first = Math.min(n, this.cap - idx);
    out.set(this.data.subarray(idx, idx + first), at);
    if (n > first) out.set(this.data.subarray(0, n - first), at + first);
    Atomics.store(this.ctrl, TAIL, (tail + n) | 0);
    Atomics.notify(this.ctrl, TAIL);
  }

  // ---- blocking (worker side) ----

  writeFrameBlocking(payload) {
    const header = new Uint8Array(4);
    new DataView(header.buffer).setUint32(0, payload.length, true);
    this._writeAllBlocking(header);
    this._writeAllBlocking(payload);
  }

  _writeAllBlocking(bytes) {
    let from = 0;
    while (from < bytes.length) {
      let space = this.free();
      while (space === 0) {
        Atomics.wait(this.ctrl, TAIL, Atomics.load(this.ctrl, TAIL));
        space = this.free();
      }
      const n = Math.min(space, bytes.length - from);
      this.copyIn(bytes, from, n);
      from += n;
    }
  }

  readFrameBlocking() {
    const header = new Uint8Array(4);
    this._readAllBlocking(header);
    const len = new DataView(header.buffer).getUint32(0, true);
    const payload = new Uint8Array(len);
    this._readAllBlocking(payload);
    return payload;
  }

  _readAllBlocking(out) {
    let at = 0;
    while (at < out.length) {
      let avail = this.used();
      while (avail === 0) {
        Atomics.wait(this.ctrl, HEAD, Atomics.load(this.ctrl, HEAD));
        avail = this.used();
      }
      const n = Math.min(avail, out.length - at);
      this.copyOut(out, at, n);
      at += n;
    }
  }

  // ---- async (main-thread side) ----

  async writeFrame(payload) {
    const header = new Uint8Array(4);
    new DataView(header.buffer).setUint32(0, payload.length, true);
    await this._writeAll(header);
    await this._writeAll(payload);
  }

  async _writeAll(bytes) {
    let from = 0;
    while (from < bytes.length) {
      let space = this.free();
      while (space === 0) {
        const r = Atomics.waitAsync(
          this.ctrl,
          TAIL,
          Atomics.load(this.ctrl, TAIL),
        );
        if (r.async) await r.value;
        space = this.free();
      }
      const n = Math.min(space, bytes.length - from);
      this.copyIn(bytes, from, n);
      from += n;
    }
  }

  async readFrame() {
    const header = new Uint8Array(4);
    await this._readAll(header);
    const len = new DataView(header.buffer).getUint32(0, true);
    const payload = new Uint8Array(len);
    await this._readAll(payload);
    return payload;
  }

  async _readAll(out) {
    let at = 0;
    while (at < out.length) {
      let avail = this.used();
      while (avail === 0) {
        const r = Atomics.waitAsync(
          this.ctrl,
          HEAD,
          Atomics.load(this.ctrl, HEAD),
        );
        if (r.async) await r.value;
        avail = this.used();
      }
      const n = Math.min(avail, out.length - at);
      this.copyOut(out, at, n);
      at += n;
    }
  }
}
