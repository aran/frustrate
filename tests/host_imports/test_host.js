// The `frustrate_test_host` import namespace of //tests/host_imports. Every
// realm that instantiates the module imports its own copy of this file, so the
// state below is per realm.
//
// An actor's realm opens a file in the origin-private file system and keeps a
// synchronous access handle on it; every other realm has no file, and `store`
// and `load` answer -2 there.

let memory = null;
let actor = null;
let isAttached = false;
let postedDuringAttach = false;
let file = null;

const bytes = (ptr, len) => new Uint8Array(memory.buffer, ptr, len);

export async function attach(instance) {
  memory = instance.memory;
  actor = instance.actor;
  // A Worker's runtime tells the page it is ready with postMessage, and must
  // not before this attach has finished. Watching the global it calls makes a
  // runtime that did not await attach observable here, whatever the timing.
  const post = globalThis.postMessage;
  globalThis.postMessage = function (...args) {
    postedDuringAttach = true;
    return post.apply(this, args);
  };
  try {
    if (actor !== null) {
      const root = await navigator.storage.getDirectory();
      // A sync access handle locks its file, so each Worker gets its own.
      const name = 'host-imports-' + actor.type + '-' + crypto.randomUUID();
      const handle = await root.getFileHandle(name, { create: true });
      file = await handle.createSyncAccessHandle();
    }
  } finally {
    globalThis.postMessage = post;
  }
  isAttached = true;
}

export function actor_json(ptr, cap) {
  if (actor === null) return -1;
  const encoded = new TextEncoder().encode(JSON.stringify(actor));
  if (encoded.length > cap) throw new Error('actor name longer than ' + cap);
  bytes(ptr, encoded.length).set(encoded);
  return encoded.length;
}

// 1 once attach has finished, if the realm posted nothing while it ran.
export function attached() {
  return isAttached && !postedDuringAttach ? 1 : 0;
}

export function store(ptr, len) {
  if (file === null) return -2;
  file.truncate(0);
  // A copy, because the memory may be shared and the copy is not.
  const written = file.write(bytes(ptr, len).slice(), { at: 0 });
  file.flush();
  return written;
}

export function load(ptr, cap) {
  if (file === null) return -2;
  const buf = new Uint8Array(Math.min(cap, file.getSize()));
  const read = file.read(buf, { at: 0 });
  bytes(ptr, read).set(buf.subarray(0, read));
  return read;
}
