/// Attach meaning to a wasm trap that reached Dart with no Rust panic message
/// behind it.
///
/// The web runtime's JS-owned frame turns a trap into a
/// `BridgePanicException`. When the module `panic!`d, the `frustrate.panic`
/// hook shipped the message first and the exception carries it. When the module
/// **trapped or aborted** instead, no hook ran and nothing recorded a cause —
/// all that survives is the engine's own text, and for the two shapes this
/// runtime expects that text names nothing the reader can act on:
///
///   * `Atomics.wait cannot be called in this context` names a JS API the app
///     never called. The actual mistake is a bridged body that waited on a
///     synchronization primitive while running on the browser main thread.
///   * `unreachable` names nothing at all. On single-threaded web the expected
///     source is std's `no_threads` `RwLock`, which calls `rtabort!` on
///     reentrant access. That reason is not lost to wasm — it is lost to the
///     *stock* std, whose `panic_output()` is `None`, so `rtprintpanic!` formats
///     the string and drops it. frustrate's custom std answers `Some(Stderr)`
///     and the same string reaches `console.error`, so the guess below is only
///     needed on a build without the `stdio` facility.
///
/// Both are already *loud* — the call throws and reaches
/// `PlatformDispatcher.onError` like any other error. Neither is
/// *attributable*, and both are mistakes a declaration would have caught
/// (`native_only`, `on_contention`). This is the attribution.
///
/// **This is a heuristic and is built as one.** The engine's message is
/// preserved verbatim and prefixed, never replaced, so an unrecognised or
/// reworded trap degrades to exactly the behaviour that predates this file. The
/// quoted strings are Chromium's, the engine this repo's browser suites run;
/// another engine wording them differently costs the explanation, not the
/// report.
///
/// It lives apart from `runtime_web.dart` because it is pure `String` → `String`
/// with no `dart:js_interop` in it, which is the difference between a unit test
/// on the VM and one that needs a browser and a real trap to reach.
library;

/// The main-thread wait. Every std primitive that waits — `park`, a contended
/// `Mutex`/`RwLock`, `Condvar`, `thread::sleep` — lowers to
/// `memory.atomic.wait32` on threaded wasm, and the spec bars it on the main
/// thread.
const String _atomicsWait = 'Atomics.wait';

/// An abort that bypassed the panic hook, so no message survived.
const String _unreachable = 'unreachable';

/// Prefix [engine] with what it means, when it is a shape we recognise.
/// Returns [engine] unchanged otherwise.
String explainTrap(String engine) {
  if (engine.contains(_atomicsWait)) {
    return '$engine\n'
        'A bridged call waited on a synchronization primitive while running on '
        'the browser main thread, which the platform forbids. Something in the '
        'body blocked: a lock, a channel receive, a `block_on`, or a '
        '`thread::sleep`.\n'
        'A #[bridge(sync)] body runs on the calling thread, so it must not '
        'block. Make it an `async fn` (the executor yields to the event loop '
        'instead of parking a thread), move the work to an Actor, or — if '
        'it can only ever run natively — declare that with '
        '#[bridge(native_only)]. It is not frustrate\'s own lock: a locked '
        'type\'s sync try-lock (on_contention = "error") never waits, on any '
        'target, and on_contention = "block" is refused here already.';
  }
  if (engine.contains(_unreachable)) {
    return '$engine\n'
        'The Rust module aborted without a panic message. On single-threaded '
        'web the usual cause is a std RwLock used reentrantly — `read()` while '
        'a write is held, or `write()` while anything is held. It aborts '
        'through `rtabort!`, and the stock wasm std drops the reason because '
        'it has nowhere to write it. Rebuild with the custom std\'s `stdio` '
        'facility and that reason arrives in the browser console verbatim — '
        'better than this guess, which knows only '
        'that something aborted.';
  }
  return engine;
}
