/// How many **wasm entries** one `callSync` costs.
///
/// Under dart2wasm, Dart *is* wasm, so every `dart:js_interop` operation is a
/// wasm->JS boundary crossing and the count of entries per bridge call is the
/// thing that sets the crossing floor. With the response slab it is **three**
/// for a response that fits and **four** for one that does not.
///
/// This file exists because that is a structural property no timing test can
/// pin. A benchmark measures it indirectly, drifts run to run, and cannot be a
/// `bazel test` target; a counter around the one JS frame every entry goes
/// through (`$frustrateCall` — see `runtime/dart/lib/src/js/frustrate.js`)
/// measures it exactly, deterministically, and fails loudly the day someone
/// puts an allocation or a free back on the path.
///
/// `csp_glue_test.dart` uses the same counting shape to instrument
/// `URL.createObjectURL`.
///
/// The overflow arm is also the **leak fence** for the lease path: a response
/// that overflows the slab is a Rust `Vec` the transport must hand back, and
/// forgetting to do so would read as three entries here, not four.
///
/// Browser-only: on native an FFI call is a call, not a boundary crossing, and
/// there is no frame to count.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_web.dart';

/// Wasm entries seen since the last reset.
int _entries = 0;

/// The runtime's own frame, kept so the counter can delegate and so tear-down
/// can put it back.
late final JSFunction _original;

/// Wrap `globalThis.$frustrateCall` with a counting delegate.
///
/// `apply` rather than `callAsFunction`: the frame takes seven parameters
/// (the export plus its six arguments) and `callAsFunction` tops out at four.
void _instrumentCallFrame() {
  final frame = globalContext.getProperty<JSFunction>(r'$frustrateCall'.toJS);
  _original = frame;
  globalContext.setProperty(
    r'$frustrateCall'.toJS,
    ((
          JSFunction f,
          JSAny? a,
          JSAny? b,
          JSAny? c,
          JSAny? d,
          JSAny? e,
          JSAny? g,
        ) {
          _entries++;
          return frame.callMethodVarArgs<JSAny?>('apply'.toJS, <JSAny?>[
            null,
            <JSAny?>[f, a, b, c, d, e, g].toJS,
          ]);
        })
        .toJS,
  );
}

/// Entries spent by [calls] runs of [body], measured after one warm-up run so
/// nothing first-call (a `late final` export lookup, a lazily built writer) is
/// counted as per-call cost.
///
/// The warm-up runs [body] too, so a caller accumulating a side effect sees
/// `calls + 1` of them — which the sanity assertions below spell out rather
/// than hide.
int _entriesPer(int calls, void Function() body) {
  body();
  _entries = 0;
  for (var i = 0; i < calls; i++) {
    body();
  }
  return _entries;
}

void main() {
  setUpAll(() async {
    await initBridge();
    _instrumentCallFrame();
  });

  tearDownAll(() {
    globalContext.setProperty(r'$frustrateCall'.toJS, _original);
  });

  test('the counter observes the runtime at all', () {
    // A positive control for every assertion below: if dart2wasm bound
    // `$frustrateCall` once at compile time instead of reading the global per
    // call, the delegate would never run and every count would be a
    // vacuously-passing zero.
    final seen = _entriesPer(10, () => addI32(a: 1, b: 2));
    expect(
      seen,
      greaterThan(0),
      reason:
          'the wrapped call frame never ran — the counts below would '
          'all be vacuous',
    );
  });

  test('a sync call whose response fits the slab costs three wasm entries', () {
    // alloc(slab + request), frustrate_call_sync, free(the one block).
    // The response is written into the caller's slab, so there is nothing to
    // hand back and no third crossing to hand it back with.
    const calls = 100;
    var sum = 0;
    final seen = _entriesPer(calls, () => sum += addI32(a: 1, b: 2));
    expect(
      sum,
      3 * (calls + 1),
      reason: 'the calls must actually have run (+1 for the warm-up)',
    );
    expect(
      seen,
      3 * calls,
      reason:
          'expected 3 wasm entries per fitting sync call, got '
          '${seen / calls}',
    );
  });

  test('a sync call whose response overflows the slab costs four', () {
    // The fourth is `frustrate_buffer_free` on the leased response — the
    // fallback the slab cannot serve. Its presence here is what says the lease
    // is not leaked.
    final payload = Uint8List(8192);
    const calls = 20;
    var bytes = 0;
    final seen = _entriesPer(
      calls,
      () => bytes += echoBytes(data: payload).length,
    );
    expect(
      bytes,
      8192 * (calls + 1),
      reason: 'the calls must actually have run (+1 for the warm-up)',
    );
    expect(
      seen,
      4 * calls,
      reason:
          'expected 4 wasm entries per overflowing sync call, got '
          '${seen / calls}',
    );
  });

  test('a sync call with no request and no response still costs three', () {
    const calls = 50;
    final seen = _entriesPer(calls, noArgsNoRet);
    expect(
      seen,
      3 * calls,
      reason: 'expected 3 wasm entries, got ${seen / calls}',
    );
  });
}
