/// `DartFunction::call_async` across the bridge, in a browser — the portable
/// callback path, which nothing in the browser suite exercised.
///
/// **Why this gap mattered.** `transform` is `async fn` + `DartFunction`, and
/// that combination is deliberately web-eligible: the native-only derivation in
/// `codegen/src/check.rs` is gated on `!f.rust_async`, because an awaited
/// invocation parks a *future* on the executor rather than a worker thread.
/// Its glue is therefore emitted on the web surface as a real implementation
/// (`transform_sum`, the blocking twin, is the throwing stub beside it).
///
/// So on threaded wasm the invocation registry behind it is contended by two
/// different threads: a **pool worker** registers the invocation, because that
/// is where the executor drains the enclosing `async fn`; the **browser main
/// thread** answers it through `frustrate_callback_respond`. That registry held
/// a `std::sync::Mutex` until it was moved to `spin::SpinLock`
/// (runtime/rust/src/callback.rs), and a contended parking lock on the main
/// thread is `RuntimeError: Atomics.wait cannot be called in this context`.
///
/// The three tests that touch `DartFunction` are all `@TestOn('vm')` —
/// isolate-death and blocking-`call` semantics that have no web meaning — so
/// the entire awaited path had no browser coverage at all. This file is the
/// missing side.
///
/// No `@TestOn`: it must run under the VM and both browser configurations,
/// because the bug it defends against exists only in the threaded one.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('an awaited Dart closure round-trips', () async {
    expect(await transform(x: 20, f: (v) => v + 1), 21);
  });

  test('many awaited closures are in flight at once', () async {
    // The fan-out is the point, not thoroughness for its own sake: one
    // invocation at a time never contends anything, and contention is the
    // whole failure mode. Concurrent calls put registrations (pool worker) and
    // responses (main thread) in the same window.
    const n = 64;
    final results = await Future.wait([
      for (var i = 0; i < n; i++) transform(x: i, f: (v) => v * 2),
    ]);
    expect(results, [for (var i = 0; i < n; i++) i * 2]);
  });

  test('a throwing closure fails its own call and strands nothing', () async {
    await expectLater(
      transform(x: 1, f: (_) => throw StateError('deliberate')),
      throwsA(isA<BridgePanicException>()),
    );
    // The registry must not be left holding the failed entry — nor, on threaded
    // wasm, its lock. A later call proves both: a wedged spin lock here would
    // hang rather than fail, which is why this assertion follows the throw
    // instead of standing alone.
    expect(await transform(x: 7, f: (v) => v + 1), 8);
  });
}
