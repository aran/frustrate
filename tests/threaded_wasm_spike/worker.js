// Threaded wasm spike worker: instantiate the shared module on the shared
// memory, init this thread's stack + TLS by hand, run the entry closure.
onmessage = async (e) => {
  const { module, memory, stackTop, tlsPtr, entryPtr } = e.data;
  try {
    const instance = await WebAssembly.instantiate(module, {
      env: { memory },
      frustrate: {
        spawn_worker: () => {
          throw new Error('nested spawn not supported in the spike');
        },
      },
    });
    instance.exports.__stack_pointer.value = stackTop;
    instance.exports.__wasm_init_tls(tlsPtr);
    instance.exports.worker_entry(entryPtr);
    postMessage({ ok: true });
  } catch (err) {
    postMessage({ ok: false, error: String(err) });
  }
};
