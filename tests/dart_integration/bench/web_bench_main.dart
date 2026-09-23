/// The in-page half of the release web benchmark. Compiled by
/// `tool/web_bench.dart` with `dart compile wasm -O2`; it is never compiled by
/// the pub `test` runner, which is the entire reason this file exists (the
/// runner hardcodes `-O0 --enable-asserts` for dart2wasm, so no release web
/// timing is obtainable from the suite).
///
/// It runs the cells, then POSTs one JSON document to `/results` carrying both
/// halves of the provenance:
///
///   * what the DRIVER built — fetched from `/provenance.json`, which the
///     driver wrote from the commands it actually ran (compiler argv, artifact
///     shas, cargo profile, git revision);
///   * what the PAGE observed — `crossOriginIsolated`, `hardwareConcurrency`,
///     `asyncIsParallel`, the measured `performance.now()` tick, asserts.
///
/// and it refuses to produce a table when the two disagree. Every abort path
/// POSTs to `/fatal` so the driver exits nonzero with the reason rather than
/// timing out with nothing. This file asserts nothing about absolute time.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/frustrate_web.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';

import 'package:bench_harness/bench_harness.dart';

// ------------------------------------------------------------- JS interop --

@JS('fetch')
external JSPromise<_Response> _fetch(JSString url, [JSObject init]);

extension type _Response(JSObject _) implements JSObject {
  external bool get ok;
  external int get status;
  external JSPromise<JSArrayBuffer> arrayBuffer();
  external JSPromise<JSString> text();
}

@JS('Uint8Array')
extension type _JsU8Array._(JSObject _) implements JSObject {
  external factory _JsU8Array(JSAny lengthOrBuffer);
  external int get length;
}

@JS('SharedArrayBuffer')
extension type _SharedArrayBuffer._(JSObject _) implements JSObject {
  external factory _SharedArrayBuffer(int byteLength);
}

@JS('performance.now')
external double _perfNow();

/// POST [body] to [path]. Fire-and-forget for logs; awaited for results.
Future<void> _post(String path, String body) async {
  final init = JSObject()
    ..setProperty('method'.toJS, 'POST'.toJS)
    ..setProperty('body'.toJS, body.toJS);
  await _fetch(path.toJS, init).toDart;
}

final List<String> _logLines = [];

/// Log to the driver. Buffered and flushed on every yield point: a hang inside
/// a synchronous batch produces no traffic at all, so the driver reports
/// "last line + silence duration" rather than waiting for a final message.
void _log(String line) {
  _logLines.add(line);
  _post('/log', line).ignore();
}

Never _fatal(String why) {
  _post('/fatal', why).ignore();
  throw StateError(why);
}

// ------------------------------------------------------------ provenance --

/// The release check. Note the direction: `assert(on = true)` sets the flag as
/// a side effect of a condition that is *true*, so it never fails. Writing it
/// the other way round (`ok = false` inside the assert) throws an
/// `AssertionError` on exactly the configuration it is meant to report.
bool _assertsEnabled() {
  var on = false;
  assert(on = true);
  return on;
}

/// Measure the real granularity of `performance.now()`, which is what
/// `Stopwatch` is on dart2wasm. A cross-origin-isolated page gets ~5 us; a
/// non-isolated one gets ~100 us. Reported so no one has to assume.
double _measureTickUs() {
  var smallest = double.infinity;
  for (var i = 0; i < 200000; i++) {
    final a = _perfNow();
    final b = _perfNow();
    final d = b - a;
    if (d > 0 && d < smallest) smallest = d;
  }
  return smallest.isFinite ? smallest * 1000 : -1;
}

// ----------------------------------------------------------------- bodies --

Uint8List _dartBytes(int n) {
  final b = Uint8List(n);
  for (var i = 0; i < n; i++) {
    b[i] = (i * 31 + 7) & 0xff;
  }
  return b;
}

/// A JS-owned `Uint8Array` of [n] bytes, over a plain ArrayBuffer.
Uint8List _jsBytes(int n) {
  final a = _JsU8Array(n.toJS) as JSUint8Array;
  final view = a.toDart;
  for (var i = 0; i < n; i++) {
    view[i] = (i * 31 + 7) & 0xff;
  }
  return view;
}

/// The same, over a SharedArrayBuffer — what `_buffer` actually is under the
/// threaded fixture (`runtime_web.dart`: `_memory` is the shared memory there).
Uint8List? _sabBytes(int n) {
  try {
    final buf = _SharedArrayBuffer(n);
    final a = _JsU8Array(buf) as JSUint8Array;
    final view = a.toDart;
    for (var i = 0; i < n; i++) {
      view[i] = (i * 31 + 7) & 0xff;
    }
    return view;
  } catch (_) {
    return null;
  }
}

void _feed(Uint8List v) {
  sentinel += v.length;
  if (v.isNotEmpty) sentinel += v.first + v.last;
}

const _sizes = <(String, int)>[
  ('0 B', 0),
  ('64 B', 64),
  ('1 KiB', 1024),
  ('64 KiB', 64 * 1024),
  ('1 MiB', 1024 * 1024),
];

/// Sizes for the encoder sweep (section E), in **code units** rather than
/// bytes — the encoders' cost tracks units, and CJK is 3 bytes per unit.
///
/// Finer than [_sizes] and concentrated low on purpose: the crossover between
/// the two encoders is what this sweep exists to locate, and locating it at 4x
/// steps is how a threshold ends up imported from the wrong engine.
const _encoderSizes = <(String, int)>[
  ('8', 8),
  ('16', 16),
  ('32', 32),
  ('64', 64),
  ('128', 128),
  ('256', 256),
  ('1 K', 1024),
  ('4 K', 4096),
  ('64 K', 65536),
  ('1 M', 1024 * 1024),
];

/// The platform UTF-8 encoder, as `codec_backing_web.dart` declares it.
@JS('TextEncoder')
extension type _JsTextEncoder._(JSObject _) implements JSObject {
  external _JsTextEncoder();
  external JSUint8Array encode(JSString source);
}

final _textEncoder = _JsTextEncoder();

/// One cell, deferred so every body can be pre-warmed before any is timed.
class _Spec {
  _Spec.sync(
    this.section,
    this.label,
    this.axis,
    void Function() body, {
    this.bytes,
    this.note,
  }) : syncBody = body,
       asyncBody = null;
  _Spec.async(
    this.section,
    this.label,
    this.axis,
    Future<void> Function() body, {
    this.bytes,
    this.note,
  }) : syncBody = null,
       asyncBody = body;

  final String section;
  final String label;
  final String axis;
  final int? bytes;
  final String? note;
  final void Function()? syncBody;
  final Future<void> Function()? asyncBody;

  Future<Cell> measure() async => syncBody != null
      ? measureSync(section, label, axis, syncBody!, bytes: bytes, note: note)
      : await measureAsync(
          section,
          label,
          axis,
          asyncBody!,
          bytes: bytes,
          note: note,
        );

  /// Pre-warm. V8 tiers wasm functions Liftoff->TurboFan on a call-count
  /// trigger, so without this the FIRST timed cell runs colder shared code (the
  /// writer, the codec, the transport) than the last, and the section-D/section-B
  /// comparison would partly be a measurement of compile order.
  Future<void> warm() async {
    for (var i = 0; i < 20; i++) {
      if (syncBody != null) {
        syncBody!();
      } else {
        await asyncBody!();
      }
    }
  }
}

// ------------------------------------------------------------------- main --

Future<void> main() async {
  try {
    await _run();
  } catch (e, st) {
    _post('/fatal', '$e\n$st').ignore();
    rethrow;
  }
}

Future<void> _run() async {
  // -- provenance, before anything is measured ------------------------------
  final provResp = await _fetch('provenance.json'.toJS).toDart;
  if (!provResp.ok) {
    _fatal(
      'web_bench: GET /provenance.json failed (${provResp.status}). '
      'The driver did not stage its build description; no cell can state '
      'its config, so none may be printed.',
    );
  }
  final built =
      jsonDecode((await provResp.text().toDart).toDart) as Map<String, Object?>;

  final asserts = _assertsEnabled();
  if (asserts) {
    _fatal(
      'web_bench: asserts are ENABLED in this module. That is not a '
      'release build — `dart compile wasm` only enables them with an '
      'explicit --enable-asserts, so the driver passed one or compiled the '
      'wrong entry point. Refusing to produce numbers.',
    );
  }

  final coi = globalContext
      .getProperty<JSBoolean?>('crossOriginIsolated'.toJS)
      ?.toDart;
  if (coi != true) {
    _fatal(
      'web_bench: crossOriginIsolated is $coi. The driver serves COOP: '
      'same-origin + COEP, so this means the headers did not arrive — '
      'SharedArrayBuffer would be unavailable and performance.now() would be '
      'coarsened to 100us. Refusing to produce numbers.',
    );
  }

  final tickUs = _measureTickUs();
  _log('provenance ok; crossOriginIsolated=$coi tick=${tickUs}us');

  // -- bring up the bridge --------------------------------------------------
  //
  // index.html loaded frustrate.js as a plain <script src>, so the glue is
  // already installed and $frustrateGlueUrl points at the served copy; workers
  // come from that URL rather than blob:, which is what makes the page's strict
  // CSP survivable. `_installGlue` early-returns on that.
  await FrustrateWeb.initFromUrl('test_api.wasm');
  checkFrustrateSchema();

  final rt = Frustrate.instance;
  final declaredThreaded = built['fixture_flavour'] == 'threaded';
  if (rt.asyncIsParallel != declaredThreaded) {
    _fatal(
      'web_bench: the driver says it staged the '
      '"${built['fixture_flavour']}" fixture, but the runtime reports '
      'asyncIsParallel=${rt.asyncIsParallel}. The module the page loaded is '
      'not the module the driver built. Refusing to produce numbers.',
    );
  }
  _log('bridge up; asyncIsParallel=${rt.asyncIsParallel}');

  final miner = await Miner.new_(label: 'bench');

  // The dispatch-route pair (section A). `Counter` is `locked`, so
  // `Counter.add` is dispatched through the cooperative executor: on
  // single-threaded web its body runs on a microtask scheduled by the
  // `schedule_drain` import, not inline during the call. `requestFloorAsync`
  // is the same call shape — a plain `fn`, async dispatch, trivial body — with
  // no lock, so it stays on `pool::spawn_call` and runs inline.
  //
  // They are here as a **pair**, and that is the whole point: they share the
  // transport, the codec and the response path, so the difference between them
  // is the route and nothing else. A single locked row would only tell you what
  // a web round trip costs today, which is not the question.
  final counter = Counter.new_();
  final floorArg = Uint8List(0);

  // -- build the spec list --------------------------------------------------
  final specs = <_Spec>[];

  // A. overhead floor.
  specs.add(
    _Spec.sync('A', 'noArgsNoRet (sync)', 'overhead', () {
      noArgsNoRet();
      sentinel++;
    }, bytes: 0),
  );
  specs.add(
    _Spec.sync('A', 'addI32 (sync)', 'overhead', () {
      sentinel += addI32(a: 1, b: 2);
    }, bytes: 0),
  );
  specs.add(
    _Spec.async(
      'A',
      'requestFloorAsync (dispatched, no lock)',
      'overhead+scheduling',
      () async {
        sentinel += await requestFloorAsync(data: floorArg);
      },
      bytes: 0,
      note:
          'The control for the row below. Plain `fn`, async dispatch, no '
          'lock, so it takes the pool arm: inline on single-threaded web, a '
          'worker elsewhere. Read the two together — alone, each is just a '
          'measurement of a web round trip.',
    ),
  );
  specs.add(
    _Spec.async(
      'A',
      'Counter.add (dispatched, locked)',
      'overhead+scheduling',
      () async {
        sentinel += await counter.add(delta: 1);
      },
      bytes: 0,
      note:
          'Same call shape as the control, one difference: the receiver is a '
          '`locked` opaque, so this dispatches through the cooperative '
          'executor and its lock acquisition happens inside the future. On '
          'single-threaded web that costs a `schedule_drain` import out to JS, '
          'a microtask turn, and a `frustrate_drain` export back in, where the '
          'control runs inline during the call. The gap between these two rows '
          'is the price of the route.',
    ),
  );
  specs.add(
    _Spec.async(
      'A',
      'Miner.calls() (actor, serial await)',
      'overhead+scheduling',
      () async {
        sentinel += await miner.calls();
      },
      bytes: 0,
      note:
          'the floor the Miner.digest rows in B sit above: one postMessage '
          'round trip through a Worker running a second instance of the bridge '
          'module, with a ~9-byte payload. Serial await, so it also pays a full '
          'event-loop turn per call.',
    ),
  );

  // D. the raw crossing primitives — no bridge, no Rust.
  for (final (label, n) in _sizes) {
    final js = _jsBytes(n);
    final heap = _dartBytes(n);

    specs.add(
      _Spec.sync(
        'D',
        'Uint8List.fromList(jsBacked) $label',
        'copy',
        () => _feed(Uint8List.fromList(js)),
        bytes: n,
        note:
            'THE DEFECT. Byte-for-byte what runtime_web.dart `_read` does to '
            'every response. `Uint8List.fromList` allocates a wasm-heap list '
            'and calls setRange, whose JSIntegerArrayBase fast path is '
            'copyToWasmI8Array — a JS `for` loop doing one exported wasm call '
            'PER BYTE. Confirmed present in the emitted .mjs at -O2.',
      ),
    );

    specs.add(
      _Spec.sync(
        'D',
        'jsBacked.sublist(0, n) $label',
        'copy',
        () => _feed(js.sublist(0, n)),
        bytes: n,
        note:
            'the proposed replacement: JSUint8ArrayImpl.sublist is '
            'buffer.cloneAsDataView, a single bulk `new Uint8Array(dst).set(src)` '
            'in JS. NOTE it returns a still-JS-BACKED list, not a wasm-heap '
            'Uint8List — so it moves the bytes cheaply but leaves subsequent '
            'element reads on the JS side. Read it together with the two '
            '"read every byte" rows below before concluding it is a drop-in.',
      ),
    );

    specs.add(
      _Spec.sync(
        'D',
        'dartHeapList.toJS $label',
        'copy',
        () {
          final j = heap.toJS;
          sentinel += j.toDart.length;
        },
        bytes: n,
        note:
            'the REQUEST direction, and the one the actor path also pays '
            '(runtime_web.dart `_WebActorHost.call` does `req.toJS`). '
            'jsUint8ArrayFromDartUint8List -> _copyFromWasmI8Array: the same '
            'per-byte JS loop as the defect, running the other way.',
      ),
    );

    if (n > 0) {
      specs.add(
        _Spec.sync('D', 'read every byte, JS-backed $label', 'read', () {
          var s = 0;
          for (var i = 0; i < n; i++) {
            s += js[i];
          }
          sentinel += s;
        }, bytes: n),
      );
      specs.add(
        _Spec.sync('D', 'read every byte, Dart-heap $label', 'read', () {
          var s = 0;
          for (var i = 0; i < n; i++) {
            s += heap[i];
          }
          sentinel += s;
        }, bytes: n),
      );

      final jsBd = js.buffer.asByteData();
      final heapBd = heap.buffer.asByteData();
      final n8 = (n ~/ 8) * 8;
      final n4 = (n ~/ 4) * 4;
      specs.add(
        _Spec.sync(
          'D',
          'ByteData.getInt64 loop, JS-backed $label',
          'read',
          () {
            var s = 0;
            for (var i = 0; i + 8 <= n8; i += 8) {
              s += jsBd.getInt64(i, Endian.little);
            }
            sentinel += s & 0xffff;
          },
          bytes: n,
          note:
              'the codec\'s i64 accessor. On a JS-backed ByteData this is '
              'DataView.prototype.getBigInt64 (SDK js_typed_array.dart), i.e. a '
              'JS BigInt allocated and unboxed per element; on a Dart-heap '
              'ByteData it is a plain wasm i64 load. This pair is why "just '
              'keep the response JS-backed" is not automatically a win.',
        ),
      );
      specs.add(
        _Spec.sync('D', 'ByteData.getInt64 loop, Dart-heap $label', 'read', () {
          var s = 0;
          for (var i = 0; i + 8 <= n8; i += 8) {
            s += heapBd.getInt64(i, Endian.little);
          }
          sentinel += s & 0xffff;
        }, bytes: n),
      );
      specs.add(
        _Spec.sync(
          'D',
          'ByteData.getUint32 loop, JS-backed $label',
          'read',
          () {
            var s = 0;
            for (var i = 0; i + 4 <= n4; i += 4) {
              s += jsBd.getUint32(i, Endian.little);
            }
            sentinel += s & 0xffff;
          },
          bytes: n,
        ),
      );
      specs.add(
        _Spec.sync(
          'D',
          'ByteData.getUint32 loop, Dart-heap $label',
          'read',
          () {
            var s = 0;
            for (var i = 0; i + 4 <= n4; i += 4) {
              s += heapBd.getUint32(i, Endian.little);
            }
            sentinel += s & 0xffff;
          },
          bytes: n,
        ),
      );
    }
  }

  // D2. the same copy primitives over a SharedArrayBuffer. Under the threaded
  // fixture the bridge's linear memory IS a SAB, so D's plain-ArrayBuffer rows
  // generalize exactly to single-threaded and only approximately to threaded.
  for (final (label, n) in _sizes) {
    final sab = _sabBytes(n);
    if (sab == null) continue;
    specs.add(
      _Spec.sync(
        'D2',
        'Uint8List.fromList(sabBacked) $label',
        'copy',
        () => _feed(Uint8List.fromList(sab)),
        bytes: n,
        note:
            'SharedArrayBuffer-backed twins of the section-D copy rows. The '
            'per-byte loops are indifferent to shared-ness; the BULK paths are '
            'not necessarily, which is the only reason these exist. Under the '
            'threaded fixture this is the shape the real transport sees.',
      ),
    );
    specs.add(
      _Spec.sync(
        'D2',
        'sabBacked.sublist(0, n) $label',
        'copy',
        () => _feed(sab.sublist(0, n)),
        bytes: n,
      ),
    );
    if (n > 0) {
      specs.add(
        _Spec.sync('D2', 'read every byte, SAB-backed $label', 'read', () {
          var s = 0;
          for (var i = 0; i < n; i++) {
            s += sab[i];
          }
          sentinel += s;
        }, bytes: n),
      );
    }
  }

  // B. the transport, against the real fixture.
  for (final (label, n) in _sizes) {
    final payload = _dartBytes(n);
    specs.add(
      _Spec.sync(
        'B',
        'sumBytes $label (sync, i64 response)',
        'codec',
        () {
          sentinel += sumBytes(data: payload);
        },
        bytes: n,
        note:
            'REQUEST-side only: the response is one i64. sumBytes vs '
            'echoBytes at the same size is therefore a clean read on the '
            'RESPONSE half — which is the half `_read` (section D row 1) '
            'dominates.',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final payload = _dartBytes(n);
    specs.add(
      _Spec.sync(
        'B',
        'echoBytes $label (sync, request + response)',
        'codec',
        () {
          _feed(echoBytes(data: payload));
        },
        bytes: n,
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final payload = _dartBytes(n);
    specs.add(
      _Spec.sync(
        'B',
        'revBytes $label (sync, request + response)',
        'codec',
        () {
          _feed(revBytes(data: payload));
        },
        bytes: n,
        note:
            'echoBytes with a real Rust body (a reversing collect) in the '
            'middle. The echoBytes/revBytes delta is the Rust-side work; '
            'everything else about the two rows is identical.',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final s = 'a' * n;
    specs.add(
      _Spec.sync(
        'B',
        'echoString $label ASCII (sync)',
        'codec',
        () {
          final r = echoString(s: s);
          sentinel += r.length;
          if (r.isNotEmpty) sentinel += r.codeUnitAt(0);
        },
        bytes: n,
        note:
            'NOT the same measurement as the native driver\'s row of this '
            'name. Under dart2wasm a Dart String is a JS string, so this is '
            'JSStringImpl <-> UTF-8 machinery (the SDK\'s JS-side encode plus '
            'Rust-side validation), not a walk over a Dart-heap UTF-16 array. '
            'The CJK row is here so nobody generalizes the ASCII fast path.',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    // 3 UTF-8 bytes per CJK codepoint; size the string to hit n bytes.
    final s = '中' * (n ~/ 3);
    specs.add(
      _Spec.sync('B', 'echoString $label CJK (sync)', 'codec', () {
        final r = echoString(s: s);
        sentinel += r.length;
        if (r.isNotEmpty) sentinel += r.codeUnitAt(0);
      }, bytes: (n ~/ 3) * 3),
    );
  }
  for (final (label, n) in _sizes) {
    final xs = Int64List(n ~/ 8);
    for (var i = 0; i < xs.length; i++) {
      xs[i] = i;
    }
    specs.add(
      _Spec.sync(
        'B',
        'echoI64s $label (sync, ${xs.length} elems)',
        'codec',
        () {
          final r = echoI64s(xs: xs);
          sentinel += r.length;
          if (r.isNotEmpty) sentinel += r.first;
        },
        bytes: n,
        note:
            'element-at-a-time codec, not a bulk memcpy — and that is the '
            "runtime's choice, not codegen's: the generated code calls "
            'writeI64List/readI64List here exactly as it does natively, and '
            '`bulkI64Ok` refuses the byte copy on this backend. So each '
            'element is a getInt64 on whatever the response list is backed by '
            '— see the section-D getInt64 pair.',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final xs = Float64List(n ~/ 8);
    for (var i = 0; i < xs.length; i++) {
      xs[i] = i.toDouble();
    }
    specs.add(
      _Spec.sync(
        'B',
        'echoF64s $label (sync, ${xs.length} elems)',
        'codec',
        () {
          final r = echoF64s(xs: xs);
          sentinel += r.length;
          if (r.isNotEmpty) sentinel += r.first.toInt();
        },
        bytes: n,
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final payload = _dartBytes(n);
    specs.add(
      _Spec.async(
        'B',
        'Miner.digest $label (actor, serial await)',
        'codec+scheduling',
        () async {
          sentinel += await miner.digest(data: payload);
        },
        bytes: n,
        note:
            'READ THE SECTION-B BLURB BEFORE QUOTING THIS ROW. It is not a '
            'copy-free control: digest\'s payload rides the REQUEST, and the '
            'actor request path (`req.toJS`) is the same per-byte primitive as '
            'the sync path. The no-copy JS-backed response decode this row was '
            'expected to isolate only ever handles digest\'s 9-byte reply.',
      ),
    );
  }

  // E. the UTF-8 encoder candidates, so the threshold in
  // `codec_backing_web.dart` is DERIVED here rather than imported from another
  // engine. It was imported once, from node, and the sign was wrong: node put
  // the crossover at ~24 code units and Chrome does not agree at all.
  for (final (label, chars) in _encoderSizes) {
    for (final (kind, s) in <(String, String)>[
      ('ASCII', 'a' * chars),
      ('CJK', '日' * chars),
    ]) {
      specs.add(
        _Spec.sync(
          'E',
          'utf8.encode $kind $label',
          'encode',
          () {
            sentinel += utf8.encode(s).length;
          },
          bytes: chars,
          note:
              'the SDK encoder. Under dart2wasm a String is a JS string, so '
              'this walks it across the JS boundary one code unit at a time.',
        ),
      );
      specs.add(
        _Spec.sync(
          'E',
          'TextEncoder.encode $kind $label',
          'encode',
          () {
            sentinel += _textEncoder.encode(s.toJS).toDart.length;
          },
          bytes: chars,
          note:
              'the platform encoder, including `s.toJS` and the `.toDart` '
              'rewrap. Whether toJS is free is engine-dependent and is '
              'exactly what makes this row unpredictable from another '
              'engine\'s numbers.',
        ),
      );
      // What `writeString` actually does with each result: the encoder output
      // has to reach the writer's Dart-heap buffer, and the two candidates
      // land there by different primitives. Measuring the encoder alone would
      // overstate TextEncoder, whose result is JS-backed and so pays a
      // crossing the SDK encoder's Dart-heap result does not.
      final dst = Uint8List(chars * 3 + 8);
      specs.add(
        _Spec.sync('E', 'utf8.encode + setRange $kind $label', 'encode', () {
          final b = utf8.encode(s);
          dst.setRange(0, b.length, b);
          sentinel += b.length;
        }, bytes: chars),
      );
      specs.add(
        _Spec.sync(
          'E',
          'TextEncoder + setRange $kind $label',
          'encode',
          () {
            final b = _textEncoder.encode(s.toJS).toDart;
            dst.setRange(0, b.length, b);
            sentinel += b.length;
          },
          bytes: chars,
          note:
              'THE ROW THAT DECIDES THE THRESHOLD. Compare against '
              '"utf8.encode + setRange" at the same size and kind: this is '
              'the whole of what writeString does differently.',
        ),
      );
    }
  }

  // -- pre-warm everything, then measure ------------------------------------
  _log('specs=${specs.length}; pre-warming');
  for (final s in specs) {
    await s.warm();
  }
  _log('pre-warm done; measuring');

  final cells = <Cell>[];
  for (var i = 0; i < specs.length; i++) {
    cells.add(await specs[i].measure());
    if (i % 10 == 0 || i == specs.length - 1) {
      _log('cell ${i + 1}/${specs.length}: ${specs[i].label}');
      // Yield so buffered /log POSTs actually leave the page.
      await Future<void>.delayed(Duration.zero);
    }
  }

  await miner.dispose();

  if (sentinel == 0) {
    _fatal(
      'web_bench: sentinel is zero — results were optimized away, or '
      'every fixture returned nothing. Numbers are not usable.',
    );
  }

  // -- report ---------------------------------------------------------------
  final meta = <String, Object?>{
    'bench': 'web_bench',
    'config': declaredThreaded ? 'web-threaded' : 'web-single-threaded',
    'covers': [declaredThreaded ? 'web-threaded' : 'web-single-threaded'],
    'does_not_cover': [
      'native',
      if (declaredThreaded) 'web-single-threaded' else 'web-threaded',
    ],
    // What the driver built (compiler argv, shas, cargo profile, git rev).
    ...built,
    // What the page observed.
    'cross_origin_isolated': coi,
    'hardware_concurrency': rt.hardwareParallelism,
    'async_is_parallel': rt.asyncIsParallel,
    'dart_asserts_enabled': asserts,
    'performance_now_tick_us': roundSig(tickUs),
    'user_agent': globalContext
        .getProperty<JSObject>('navigator'.toJS)
        .getProperty<JSString>('userAgent'.toJS)
        .toDart,
    'batches_per_cell': batches,
    'target_batch_us': targetBatchUs,
    'timestamp': DateTime.now().toUtc().toIso8601String(),
    'sentinel': sentinel,
  };

  final table = renderReport(
    title:
        'frustrate web bench  (dart2wasm '
        '${built['dart_optimization_level']}, asserts off)',
    banner: _banner(meta),
    sections: _sections,
    cells: cells,
  );

  await _post(
    '/results',
    jsonEncode({
      'meta': meta,
      'cells': cells.map((c) => c.toJson()).toList(),
      'table': table,
      'log': _logLines,
    }),
  );
}

// ----------------------------------------------------------------- prose --

List<String> _banner(Map<String, Object?> meta) => [
  'config      ${meta['config']}',
  '            Says NOTHING about native, or about the other web config.',
  'dart        ${meta['dart_version']}',
  '            dart compile wasm ${meta['dart_optimization_level']}  '
      'asserts=${meta['dart_asserts_enabled']}',
  '            ${meta['dart_module_bytes']} B  '
      'sha256 ${meta['dart_module_sha256']}',
  'rust        ${meta['rust_profile']} / ${meta['fixture_flavour']}',
  '            ${meta['fixture_bytes']} B  '
      'sha256 ${meta['fixture_sha256']}',
  'source      git ${meta['git_revision']}'
      '${meta['git_dirty'] == true ? ' (DIRTY — see git_status in JSON)' : ''}',
  '            runtime_web.dart sha256 ${meta['runtime_web_sha256']}',
  '            binary_codec.dart sha256 ${meta['binary_codec_sha256']}',
  'browser     ${meta['user_agent']}',
  '            crossOriginIsolated=${meta['cross_origin_isolated']}  '
      'hardwareConcurrency=${meta['hardware_concurrency']}  '
      'asyncIsParallel=${meta['async_is_parallel']}',
  'clock       Stopwatch is 1000*performance.now() on dart2wasm; measured '
      'tick ${meta['performance_now_tick_us']} us',
];

const Map<String, String> _sections = {
  'A':
      'A. ROUND-TRIP FLOOR — axis: BRIDGE OVERHEAD\n'
      '   Payload is ~0 and the Rust body is trivial. This is the cost of\n'
      '   CROSSING, not of doing anything. Do not read it as throughput.\n'
      '   The sync rows are a direct synchronous call into the page\'s wasm\n'
      '   instance; the actor row is a Worker round trip and is a different\n'
      '   kind of number, present so the Miner rows in B have a floor.',
  'D':
      'D. RAW CROSSING PRIMITIVES — axis: SDK COPY/READ COST, NO BRIDGE\n'
      '   No Rust, no codec, no envelope: just the four SDK operations the\n'
      '   web transport is built out of. This is the decisive section — if a\n'
      '   transport change does not move these, it cannot move section B.\n'
      '   The two per-byte loops are real and survive -O2; they are emitted\n'
      '   into the .mjs as JS `for` loops calling an exported wasm function\n'
      r'   once per byte (`$wasmI8ArrayGet` / `$wasmI8ArraySet`, from the'
      '\n'
      '   SDK\'s _copyFromWasmI8Array / copyToWasmI8Array).\n'
      '   Buffers are built ONCE, outside the timed region, so every row is\n'
      '   flattered by a cache-hot source; the ALLOCATION of each row\'s\n'
      '   result stays inside, because that is a real per-call cost.\n'
      '   These rows use a plain ArrayBuffer. See D2 for the shared case.',
  'D2':
      'D2. THE SAME, OVER A SharedArrayBuffer — axis: SDK COPY/READ COST\n'
      '   Under the THREADED fixture the bridge\'s linear memory is a\n'
      '   SharedArrayBuffer, so section D generalizes exactly to the\n'
      '   single-threaded config and only approximately to the threaded one.\n'
      '   Per-byte loops are indifferent to shared-ness; bulk paths need not\n'
      '   be. If D and D2 agree, quote D and forget this section exists.',
  'E':
      'E. UTF-8 ENCODER CANDIDATES — axis: ENCODE COST, NO BRIDGE\n'
      '   Where the threshold in codec_backing_web.dart comes from. Quote the\n'
      '   "+ setRange" pair, not the bare encoders: TextEncoder returns a\n'
      '   JS-backed list and utf8.encode a Dart-heap one, so they land in the\n'
      '   writer\'s buffer by different primitives, and comparing the encoders\n'
      '   alone flatters TextEncoder by the crossing it still owes.\n'
      '   ASCII and CJK are both swept because they scale differently: the\n'
      '   SDK cost tracks CODE UNITS and the crossing tracks BYTES, and CJK\n'
      '   is 3 bytes per unit — so a threshold fitted on ASCII alone is not\n'
      '   the same threshold.\n'
      '   This section exists because the constant was first taken from node,\n'
      '   where the crossover is ~24 units, and Chrome disagreed by enough to\n'
      '   flip the sign on real echoString rows. Re-derive here, in the\n'
      '   engine that ships, before changing it.',
  'B':
      'B. TRANSPORT AGAINST THE REAL FIXTURE — axis: CODEC + TRANSPORT\n'
      '   Payload buffers are built ONCE, outside the timed region (safe: the\n'
      '   writer copies). Response allocation stays INSIDE, because that is a\n'
      '   real cost of every call.\n'
      '   sumBytes returns an i64, so sumBytes-vs-echoBytes at the same size\n'
      '   is a clean read on the RESPONSE half — the half section D row 1\n'
      '   (`Uint8List.fromList(jsBacked)`) dominates. revBytes-vs-echoBytes\n'
      '   is the Rust-side work at the same transport cost.\n'
      '   Miner.digest is NOT the copy-free control it was commissioned as.\n'
      '   Its payload rides the REQUEST, and the actor request path does\n'
      '   `req.toJS` — the SAME per-byte primitive as the sync path\'s\n'
      '   `_writeBytes`. The JS-backed, copy-free response decode it was\n'
      '   meant to isolate only handles digest\'s 9-byte reply. What the row\n'
      '   is genuinely good for: it carries the request-direction per-byte\n'
      '   cost WITHOUT the response-direction one, so digest-vs-echoBytes\n'
      '   separates the two halves from the other side than sumBytes does.\n'
      '   Its buffer is transferred, not structure-cloned (postMessage with a\n'
      '   transfer list), so no hidden clone is buried in these numbers.',
};
