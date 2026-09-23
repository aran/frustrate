/// Every Dart-heap <-> JS byte crossing in the web transport is accounted for.
///
/// **The invariant.** On dart2wasm the Dart heap is WasmGC arrays, which JS
/// cannot address, so there is no bulk copy between a Dart-heap `Uint8List` and
/// a JS `ArrayBuffer` in *either* direction. Every such move is a JS `for` loop
/// calling an exported wasm function once per byte (the SDK's
/// `copyToWasmI8Array` / `_copyFromWasmI8Array`). That is a per-byte crossing
/// against a bulk copy, an order-of-magnitude difference that survives `-O2`
/// and always will, because binaryen optimizes wasm and that loop is JS.
/// `tests/dart_integration/bench/web_bench_main.dart` section D is where it
/// shows up.
///
/// **Why a source test rather than a behavioural one.** The cost is invisible:
/// nothing throws, no test goes red, and the browser suite runs at `-O0` where
/// the ratio is muddied. The only signal is a benchmark nobody runs on a routine
/// change, and by then the offending line is buried under a dozen commits. So
/// this pins the *decision* rather than the cost — a crossing may exist, but the
/// line has to say so and this file has to list it.
///
/// It cannot be a lint on absence, because two crossings are deliberate and one
/// is unavoidable:
///   * `_read`'s small-payload branch — below the crossover the per-byte loop
///     really is cheaper than `sublist`'s fixed overhead, and an unconditional
///     `sublist` puts that overhead on *every* call regardless of payload,
///     where it lands on the overhead floor rather than the payload rows
///     (`_bulkReadThreshold` in `runtime_web.dart` carries the reasoning).
///   * the request direction — `BinaryWriter` builds on the Dart heap by
///     construction, so `bytes.toJS` must clone. **Giving the writer a JS-backed
///     buffer was measured and declined; do not re-derive it.** It removes the
///     crossing for a *JS-backed* argument — which today is the slower of the
///     two, because `writeBytes` copies it into the Dart-heap writer and then
///     the whole request crosses anyway. But it is worth little for a
///     Dart-heap argument, costs something in one band, and turns on two
///     engine-derived thresholds. The gain is conditional on data
///     provenance the runtime cannot detect — the SDK exposes no public
///     predicate for "is this list JS-backed", and `toJS` cannot probe because it
///     is the expensive operation exactly when the answer is no. A Dart-heap
///     argument stays irreducible regardless: JS cannot address WasmGC arrays.
///
///     `TextEncoder` for `writeString` produces a JS-backed result, and it is
///     referenced rather than copied into the writer's buffer — copying it
///     there would be a crossing of exactly the kind this file refuses, one
///     per byte of every string. Referenced, it reaches the transport with its
///     backing intact and crosses once.
///   * `init(Uint8List)` — free for every caller in this repo, because
///     `initFromUrl` hands it a JS-backed list and `toJS` then unwraps. A caller
///     that supplies a Dart-heap list (a Flutter `rootBundle` load) pays the
///     per-byte crossing over the whole module. Allowlisted, not fixed, because
///     the fix belongs at the call site.
///
/// **Fail-closed.** The detector does not try to recognise byte-carrying
/// expressions — it recognises the *scalar* ones, which are enumerable, and
/// treats everything else as a suspected crossing. So a future `payload.toJS`
/// trips this test without anyone having thought to add `payload` to a list.
/// That is the whole design: the failure mode of a tripwire must be a false
/// alarm costing one comment, never a silent miss costing a per-byte
/// crossing forever.
@TestOn('vm')
library;

import 'dart:io';

import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// The justification a deliberate crossing must carry, on the same line.
const String _marker = '// per-byte:';

/// Receivers whose `.toJS` is an `int`, `bool`, or `String` conversion — cheap,
/// and not a byte move. Anything NOT here is treated as a suspected crossing;
/// see the fail-closed note above. Adding a name is a deliberate act: it must be
/// a scalar, and if you are unsure it is not.
const Set<String> _scalarReceivers = {
  // ints and bools crossing as JS numbers/booleans
  'fnId', 'callId', 'ptr', 'reqPtr', 'out', 'size', 'align', 'tlsPtr',
  'entryPtr', 'id', 'raw', 'true', 'false', 'registryIndex', 'base',
  'reqLen', 'pieceOff', '0',
  '_sharedMemoryInitialPages', '_sharedMemoryMaxPages',
  '_poolWorkerStackBytes',
  // Strings crossing as JS strings
  'frustrateGlueSource', 'url', 'servedUrl', 'name', 'type', 'args',
  '_workerRegistryKey',
  // Functions crossing as JS callables. Named rather than written as an
  // inline closure only because `toJS` rejects an all-throws closure (its
  // return type infers to `Never`), which is what puts them in this scan's
  // way at all — they move code, never bytes.
  'stackChkFail',
};

/// Every byte crossing allowed to exist, as the exact trimmed source line.
///
/// Full lines on purpose: a match is then unambiguous, and touching the
/// surrounding code forces a look at this list.
const List<String> _allowedCrossings = [
  "return Uint8List.fromList(small); // per-byte: small payloads only",
  "view.callMethod('set'.toJS, bytes.toJS); // per-byte: request direction",
  "final moduleBytes = wasmModuleBytes.toJS; // per-byte: Dart-heap init only",
  "final jsPiece = piece.toJS; // per-byte: Dart-heap piece only",
];

String _srcPath(String relative) {
  if (Platform.environment.containsKey('TEST_SRCDIR')) {
    return Runfiles.create().rlocation('_main/runtime/dart/$relative');
  }
  return relative;
}

/// Source lines that move bytes between the Dart heap and a JS `ArrayBuffer`.
List<({int number, String text})> _crossingLines(String source) {
  final out = <({int number, String text})>[];
  final lines = source.split('\n');
  var inBlockComment = false;
  for (var i = 0; i < lines.length; i++) {
    final t = lines[i].trim();
    if (inBlockComment) {
      if (t.contains('*/')) inBlockComment = false;
      continue;
    }
    if (t.startsWith('/*')) {
      if (!t.contains('*/')) inBlockComment = true;
      continue;
    }
    if (t.startsWith('//')) continue;

    // Drop string literals first: `'buf'.toJS` is a property name, not bytes,
    // and it appears on nearly every interop line in this file.
    final code = t.replaceAll(RegExp(r"'[^']*'"), "''");

    var isCrossing = code.contains('Uint8List.fromList(');
    for (final m in RegExp(r'([A-Za-z_$][\w$]*)\.toJS').allMatches(code)) {
      if (!_scalarReceivers.contains(m.group(1))) isCrossing = true;
    }
    if (isCrossing) out.add((number: i + 1, text: t));
  }
  return out;
}

void main() {
  group('runtime_web.dart', () {
    late String source;
    setUpAll(() {
      source = File(_srcPath('lib/src/runtime_web.dart')).readAsStringSync();
    });

    test('every byte crossing carries a justification', () {
      final unmarked = _crossingLines(source)
          .where((l) => !l.text.contains(_marker))
          .map((l) => '${l.number}: ${l.text}')
          .toList();
      expect(
        unmarked,
        isEmpty,
        reason:
            'byte crossing(s) with no "$_marker" justification. Moving '
            'bytes between the Dart heap and a JS ArrayBuffer costs '
            'a per-byte crossing rather than a bulk copy, and nothing else in '
            'the suite notices. If deliberate, mark the line and add it to '
            '_allowedCrossings. If the receiver is an int/bool/String rather '
            'than bytes, add its name to _scalarReceivers instead.',
      );
    });

    test('the allowlist matches the code exactly, in both directions', () {
      // Both directions matter. An addition means an unreviewed crossing; a
      // removal means the list has rotted into a description of code that no
      // longer exists, and a stale entry is what lets the next one in
      // unnoticed.
      expect(
        _crossingLines(source).map((l) => l.text).toSet(),
        _allowedCrossings.toSet(),
      );
    });

    test('the small-payload branch keeps the threshold that justifies it', () {
      // The allowlisted `fromList` is defensible only because a threshold
      // bounds it. If the constant goes while the branch stays, that allowlist
      // entry silently starts excusing a per-byte copy of every response.
      expect(
        source,
        contains('_bulkReadThreshold'),
        reason:
            "_read's allowlisted per-byte branch is justified by a "
            'measured crossover; the constant recording it is gone',
      );
    });
  });

  test('the shared codec stays platform-neutral', () {
    // `binary_codec.dart` is exported unconditionally from frustrate.dart and
    // compiled for both the VM and the web. That neutrality has been true by
    // convention and by nothing else. A `dart:js_interop` import would break
    // the native build; a `dart:ffi` one would break web — and dart2wasm
    // exposes dart:ffi partially enough that it might not fail loudly, which is
    // the trap frustrate.dart's own header warns about.
    final codec = File(_srcPath('lib/src/binary_codec.dart'))
        .readAsStringSync();
    for (final forbidden in ['dart:js_interop', 'dart:ffi', 'dart:html']) {
      expect(
        codec,
        isNot(contains("import '$forbidden")),
        reason: 'the shared codec must not import $forbidden',
      );
    }
  });
}
