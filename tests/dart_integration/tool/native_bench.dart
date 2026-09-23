/// The native (dylib/FFI) bridge benchmark — a `dart_binary`, so it is AOT
/// with asserts off, run against a stock `cargo --release` bridge.
///
///     bazel run //tests/dart_integration:native_bench
///     (cd tests/dart_integration && dart run tool/native_bench.dart)  # JIT
///
/// **Config coverage: native only.** Nothing here says anything about the
/// three web configs (single-threaded, threaded, web+SAB). The browser suite
/// cannot produce a release timing at all — the pub `test` package hardcodes
/// `-O0 --enable-asserts` for dart2wasm — so a web bench needs its own vehicle,
/// which is `tool/web_bench.dart`, and must never share a table with these
/// numbers. The two drivers share only the harness (`package:bench_harness`);
/// each owns its own prose, because almost none of the prose transfers.
///
/// Why this is not a `bazel test` target, and why it does not depend on
/// `//tests/test_api:test_api_shared`: that artifact is built at whatever
/// `--compilation_mode` the invocation carries, and Bazel's default,
/// `fastbuild`, is `-Copt-level=0` under rules_rust (opt levels are
/// `{dbg: 0, fastbuild: 0, opt: 3}`). Depending on it would mean a plain
/// `bazel run` silently benchmarks unoptimized Rust. Instead this driver runs
/// `cargo build --release` itself and loads the artifact path cargo reports,
/// so neither a stale build nor a `CARGO_TARGET_DIR` redirect can substitute a
/// different library for the one just built.
///
/// It asserts nothing about absolute time. Adding an absolute-timing assertion
/// to `bazel test //...` is a separate decision on a separate day; this driver
/// exists so that decision can be made against real numbers.
library;

import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';

import 'package:bench_harness/bench_harness.dart';

// ------------------------------------------------------------- artifacts --

/// Resolve the workspace root and verify it really is one. Under `bazel run`
/// Bazel sets BUILD_WORKSPACE_DIRECTORY; under `dart run` we walk up from the
/// script. Either way the result is checked for the workspace `Cargo.toml`, so
/// a wrong guess is a loud failure and not a benchmark of some other checkout.
({String path, String how}) _workspace() {
  final env = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final candidates = <({String path, String how})>[
    if (env != null) (path: env, how: 'BUILD_WORKSPACE_DIRECTORY'),
    (
      path: File.fromUri(Platform.script).parent.parent.parent.parent.path,
      how: 'Platform.script',
    ),
  ];
  for (final c in candidates) {
    final manifest = File('${c.path}/Cargo.toml');
    if (manifest.existsSync() &&
        manifest.readAsStringSync().contains('[workspace]')) {
      return (path: Directory(c.path).resolveSymbolicLinksSync(), how: c.how);
    }
  }
  stderr.writeln(
    'native_bench: could not locate the frustrate workspace '
    '(no Cargo.toml with [workspace] at any candidate root).\n'
    'Run it as `bazel run //tests/dart_integration:native_bench`, or as '
    '`dart run tool/native_bench.dart` from tests/dart_integration.',
  );
  exit(2);
}

/// Build the release bridge and return the dylib path **cargo reported**,
/// rather than a path assembled from `target/release/`. The difference matters:
/// a `CARGO_TARGET_DIR` or `build.target-dir` override sends the fresh build
/// elsewhere, and a guessed path would then quietly load a stale artifact —
/// right profile, old code, which is a nastier failure than loading a debug
/// build because nothing about it looks wrong.
String _buildRelease(String workspace) {
  stderr.writeln('\$ cargo build --release -p test_api  (in $workspace)');
  final r = Process.runSync('cargo', [
    'build',
    '--release',
    '-p',
    'test_api',
    '--message-format=json',
  ], workingDirectory: workspace);
  if (r.exitCode != 0) {
    stderr.write(r.stderr);
    exit(r.exitCode);
  }
  String? dylib;
  for (final line in const LineSplitter().convert(r.stdout as String)) {
    if (line.isEmpty || !line.startsWith('{')) continue;
    final msg = jsonDecode(line) as Map<String, Object?>;
    if (msg['reason'] != 'compiler-artifact') continue;
    final target = msg['target'] as Map<String, Object?>?;
    if (target?['name'] != 'test_api') continue;
    for (final f in (msg['filenames'] as List).cast<String>()) {
      if (f.endsWith('.dylib') || f.endsWith('.so') || f.endsWith('.dll')) {
        dylib = f;
      }
    }
  }
  if (dylib == null || !File(dylib).existsSync()) {
    stderr.writeln(
      'native_bench: cargo built test_api but reported no shared '
      'library artifact. Expected a cdylib from `cargo build --release -p '
      'test_api`.',
    );
    exit(2);
  }
  return dylib;
}

String? _sha256(String path) {
  for (final exe in ['shasum', 'sha256sum']) {
    try {
      final r = Process.runSync(exe, [
        if (exe == 'shasum') ...['-a', '256'],
        path,
      ]);
      if (r.exitCode == 0) {
        return (r.stdout as String).trim().split(RegExp(r'\s+')).first;
      }
    } on ProcessException {
      continue;
    }
  }
  return null;
}

bool _assertsEnabled() {
  var on = false;
  assert(on = true);
  return on;
}

String? _sysctl(String key) {
  try {
    final r = Process.runSync('sysctl', ['-n', key]);
    return r.exitCode == 0 ? (r.stdout as String).trim() : null;
  } on ProcessException {
    return null;
  }
}

// ----------------------------------------------------------------- bodies --

Uint8List _bytes(int n) {
  final b = Uint8List(n);
  for (var i = 0; i < n; i++) {
    b[i] = (i * 31 + 7) & 0xff;
  }
  return b;
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

// ------------------------------------------------------------------- main --

Future<void> main() async {
  final ws = _workspace();
  final dylibPath = _buildRelease(ws.path);
  final dylib = DynamicLibrary.open(dylibPath);

  // Raw FFI baseline: the dart:ffi trampoline with none of frustrate's
  // machinery behind it. Without this row the sync floor is unfalsifiable —
  // there is no way to tell how much of it is "calling into Rust at all".
  final rawAbi = dylib.lookupFunction<Uint64 Function(), int Function()>(
    'frustrate_runtime_abi',
  );

  FrustrateNative.initWithLibrary(dylib);
  checkFrustrateSchema();

  final cells = <Cell>[];

  // -- A. round-trip floor -------------------------------------------------
  cells.add(
    measureSync(
      'A',
      'raw dart:ffi (frustrate_runtime_abi)',
      'overhead',
      () => sentinel += rawAbi(),
      bytes: 0,
      note: 'no codec, no envelope, no allocation',
    ),
  );
  cells.add(
    measureSync('A', 'noArgsNoRet (sync)', 'overhead', () {
      noArgsNoRet();
      sentinel++;
    }, bytes: 0),
  );
  cells.add(
    measureSync('A', 'addI32 (sync)', 'overhead', () {
      sentinel += addI32(a: 1, b: 2);
    }, bytes: 0),
  );

  // The same floor with an opaque receiver. `addI32` above is a free function
  // and never touches a handle, so it cannot see what a handle costs: every
  // receiver and every handle parameter goes through `handleValue`.
  // `lenChars` is the cheapest sync body on a Confined handle, so this row
  // minus `addI32` is the receiver decode and the dispose test in front of it.
  // Not the bridge-identity check: that is inside an `assert` and this binary
  // has none.
  final benchDoc = TextDoc.new_();
  cells.add(
    measureSync(
      'A',
      'TextDoc.lenChars (sync, opaque receiver)',
      'overhead',
      () {
        sentinel += benchDoc.lenChars();
      },
      bytes: 0,
      note:
          'the sync floor with a handle in front of it. Against addI32, the '
          'difference is the receiver decode and handleValue\'s dispose '
          'test',
    ),
  );

  // The other half of what a handle costs: making one and letting it go. Every
  // mint reads the drop hook off the active transport
  // (`Frustrate.instance.handleDrop('frustrate_drop_TextDoc')`) and attaches a
  // NativeFinalizer; `dispose()` detaches it and calls the drop export. Against
  // `addI32` the difference is that whole lifecycle, which is where a per-mint
  // cost would show — `lenChars` above cannot see it, because it reuses one
  // handle forever.
  cells.add(
    measureSync(
      'A',
      'TextDoc.new_() + dispose() (mint and release)',
      'overhead',
      () {
        final d = TextDoc.new_();
        sentinel += d.lenChars();
        d.dispose();
      },
      bytes: 0,
      note:
          'one handle\'s whole life: the constructing call, the drop-hook '
          'read, the finalizer attach, one use, the detach and the drop '
          'export. Two bridge crossings, so read it against 2x addI32',
    ),
  );
  // The locked model's synchronous acquisition, uncontended: one lock, one
  // guard, one release, with nothing queued. Against `TextDoc.lenChars` the
  // difference is the lock and nothing else — same crossing, same receiver
  // decode, same trivial body — which is the only way to see what the lock
  // itself costs. `tryBump` is the write side of the same measurement.
  final benchCounter = Counter.new_();
  cells.add(
    measureSync(
      'A',
      'Counter.tryGet() (sync try-read on a locked handle)',
      'overhead',
      () {
        sentinel += benchCounter.tryGet();
      },
      bytes: 0,
      note:
          'read against TextDoc.lenChars: the difference is one uncontended '
          'read guard, taken and dropped on the calling thread',
    ),
  );
  cells.add(
    measureSync(
      'A',
      'Counter.tryBump(0) (sync try-write on a locked handle)',
      'overhead',
      () {
        sentinel += benchCounter.tryBump(by: 0);
      },
      bytes: 0,
      note:
          'the write guard beside it; a writer excludes, so this is the '
          'exclusive acquisition rather than the shared one',
    ),
  );
  cells.add(
    await measureAsync(
      'A',
      'sumSquares(0) (async pool, serial await)',
      'overhead+scheduling',
      () async {
        sentinel += await sumSquares(n: 0);
      },
      bytes: 0,
      note:
          'serial await pays a worker-thread wake AND a full Dart '
          'event-loop turn per call; that is scheduling latency, not bridge '
          'cost. Compare section A2, not the sync rows.',
    ),
  );

  final miner = await Miner.new_(label: 'bench');
  cells.add(
    await measureAsync(
      'A',
      'Miner.calls() (actor, serial await)',
      'overhead+scheduling',
      () async {
        sentinel += await miner.calls();
      },
      bytes: 0,
      note: 'as above, plus the actor host mutex and a Sender clone per call',
    ),
  );

  // -- A2. pipelined -------------------------------------------------------
  cells.add(
    await measurePipelined(
      'A2',
      'sumSquares(0) (async pool, 64 in flight)',
      'overhead',
      () async {
        sentinel += await sumSquares(n: 0);
      },
      64,
    ),
  );
  cells.add(
    await measurePipelined(
      'A2',
      'Miner.calls() (actor, 64 in flight)',
      'overhead',
      () async {
        sentinel += await miner.calls();
      },
      64,
    ),
  );

  // -- B0. request floor ---------------------------------------------------
  //
  // The instrument, not a headline. `requestFloor` is `data.len() as i64` —
  // O(1) — so these rows contain the request copies and the crossing floor
  // and nothing else. Every request-path change is judged by whether it moves
  // them.
  for (final (label, n) in _sizes) {
    final payload = _bytes(n);
    cells.add(
      measureSync(
        'B0',
        'requestFloor $label (sync)',
        'codec',
        () {
          sentinel += requestFloor(data: payload);
        },
        bytes: n,
        note:
            'the REQUEST floor: an O(1) Rust body, so unlike sumBytes this '
            'row holds no per-byte Rust work at all. Read it as request '
            'copies + crossing. sumBytes minus requestFloor at a size is the '
            'widening sum in sum_bytes',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final payload = _bytes(n);
    cells.add(
      await measurePipelined(
        'B0',
        'requestFloorAsync $label (async, 32 in flight)',
        'codec',
        () async {
          sentinel += await requestFloorAsync(data: payload);
        },
        32,
        note:
            'the same floor on the POOL path — the shape the pipelined '
            'Vec<f64> row runs on. Request-direction work translates 1:1 into '
            'that row because it runs on the calling isolate\'s single thread '
            'and no amount of pipelining overlaps it',
      ),
    );
  }

  // -- B. payload sweep ----------------------------------------------------
  for (final (label, n) in _sizes) {
    final payload = _bytes(n);
    cells.add(
      measureSync(
        'B',
        'sumBytes $label (sync, i64 response)',
        'codec',
        () {
          sentinel += sumBytes(data: payload);
        },
        bytes: n,
        note:
            'response is one i64, so sumBytes-vs-echoBytes at a size reads '
            'the RESPONSE half. Its REQUEST half is no longer echoBytes\': '
            '`sum_bytes` takes `&[u8]`, and a sync unsized borrow now decodes '
            'with ByteReader::read_bytes_borrowed() — the body reads the '
            'transport\'s buffer, with no owned Vec on the path at all. '
            'echoBytes takes an owned Vec<u8> and still pays that copy. What '
            'sumBytes has that requestFloor does not is its O(n) widening '
            'sum; the B0 delta prices it.',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final payload = _bytes(n);
    cells.add(
      measureSync(
        'B',
        'echoBytes $label (sync, request + response)',
        'codec',
        () {
          _feed(echoBytes(data: payload));
        },
        bytes: n,
        note: n >= (1 << 20)
            ? 'partly a GC/allocator benchmark: the response allocates a fresh '
                  '1 MiB Uint8List per call inside the timed region, and at this '
                  'size the VM allocates in old space, so mark-sweep work lands '
                  'in the measurement'
            : null,
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final s = 'a' * n;
    cells.add(
      measureSync(
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
            'string transcode + transport, not pure transport: UTF-8 encode '
            'in Dart, UTF-8 validation in Rust. ASCII hits the validation fast '
            'path — see the non-ASCII row',
      ),
    );
  }
  {
    // One non-ASCII datapoint, so nobody generalizes the ASCII fast path.
    final s = 'é' * 512; // 2 bytes each = 1 KiB
    cells.add(
      measureSync('B', 'echoString 1 KiB non-ASCII (sync)', 'codec', () {
        final r = echoString(s: s);
        sentinel += r.length;
        if (r.isNotEmpty) sentinel += r.codeUnitAt(0);
      }, bytes: 1024),
    );
  }
  for (final (label, n) in _sizes) {
    final xs = Int64List(n ~/ 8);
    for (var i = 0; i < xs.length; i++) {
      xs[i] = i;
    }
    cells.add(
      measureSync(
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
            'bulk memcpy both directions HERE and only here: `bulkI64Ok` '
            'keeps dart2wasm and dart2js on the element loop, so this row does '
            'not carry over to the web bench the way echoF64s does',
      ),
    );
  }
  for (final (label, n) in _sizes) {
    final xs = Float64List(n ~/ 8);
    for (var i = 0; i < xs.length; i++) {
      xs[i] = i.toDouble();
    }
    cells.add(
      measureSync(
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
    final payload = _bytes(n);
    cells.add(
      await measureAsync(
        'B',
        'Miner.digest $label (actor, serial await)',
        'codec+scheduling',
        () async {
          sentinel += await miner.digest(data: payload);
        },
        bytes: n,
        note:
            'the actor request path carries one full payload copy MORE than '
            'the sync path: frustrate_actor_call does request_slice(..).to_vec() '
            'to hand the closure ownership so it can cross to the executor '
            'thread',
      ),
    );
  }

  // -- C. work axis --------------------------------------------------------
  cells.add(
    await measureAsync(
      'C',
      'poolNthPrime(20000) (async pool)',
      'work',
      () async {
        sentinel += await poolNthPrime(n: 20000);
      },
      bytes: 0,
      note:
          'NOT a bridge number. This is Rust CPU work; the bridge is noise '
          'inside it. It is here as the contrast case for section A',
    ),
  );

  await miner.dispose();

  if (sentinel == 0) {
    stderr.writeln(
      'native_bench: sentinel is zero — results were optimized '
      'away, or every fixture returned nothing. Numbers are not usable.',
    );
    exit(3);
  }

  final size = File(dylibPath).lengthSync();
  final meta = <String, Object?>{
    'bench': 'native_bench',
    'config': 'native',
    'covers': ['native'],
    'does_not_cover': ['web-single-threaded', 'web-threaded', 'web-sab'],
    'rust_profile': 'release (stock: no lto/codegen-units tuning)',
    'bridge_path': dylibPath,
    'bridge_bytes': size,
    'bridge_sha256': _sha256(dylibPath),
    'workspace_resolved_via': ws.how,
    'dart_version': Platform.version,
    'dart_product_mode': const bool.fromEnvironment(
      'dart.vm.product',
      defaultValue: false,
    ),
    'dart_asserts_enabled': _assertsEnabled(),
    'os': Platform.operatingSystemVersion,
    'cpu': _sysctl('machdep.cpu.brand_string'),
    'cores': Platform.numberOfProcessors,
    'batches_per_cell': batches,
    'target_batch_us': targetBatchUs,
    'timestamp': DateTime.now().toUtc().toIso8601String(),
    'sentinel': sentinel,
  };

  stdout.write(
    renderReport(
      title: 'frustrate native bench',
      banner: _banner(meta),
      sections: _sections,
      cells: cells,
    ),
  );
  stdout.writeln(
    'RESULTS ${jsonEncode({...meta, 'cells': cells.map((c) => c.toJson()).toList()})}',
  );
  // The runtime holds a RawReceivePort, so the isolate would stay alive;
  // flush before exiting so a piped stdout cannot lose the RESULTS line.
  await stdout.flush();
  exit(0);
}

// ----------------------------------------------------------------- prose --
//
// The native driver's own words. `renderReport` in package:bench_harness owns the
// table mechanics and nothing else — almost none of the text below is true on
// web (there is no dylib, no FFI request block, and a Dart `String` is a JS
// string rather than a Dart-heap UTF-16 array), so the web driver writes its
// own rather than sharing these.

List<String> _banner(Map<String, Object?> meta) {
  final aot = meta['dart_product_mode'] == true;
  final asserts = meta['dart_asserts_enabled'] == true;
  return [
    'config      NATIVE only (dylib + dart:ffi).',
    '            Says NOTHING about single-threaded web, threaded '
        'web, or web+SAB.',
    'rust        ${meta['rust_profile']}',
    'bridge      ${meta['bridge_path']}',
    '            ${meta['bridge_bytes']} B  '
        'sha256 ${meta['bridge_sha256'] ?? 'unavailable'}',
    'dart        ${(meta['dart_version'] as String).split(' ').first}'
        '  product=$aot  asserts=$asserts',
    if (!aot || asserts) ...[
      '  !! NOT A RELEASE DART RUN. product=$aot asserts=$asserts.',
      '  !! Run `bazel run //tests/dart_integration:native_bench` '
          '(dart_binary => AOT, asserts off).',
      '  !! Numbers below are the Dart side unoptimized; do not '
          'quote them.',
    ],
    'machine     ${meta['cpu'] ?? '?'} / ${meta['cores']} cores / '
        '${meta['os']}',
  ];
}

const Map<String, String> _sections = {
  'B0':
      'B0. REQUEST FLOOR — axis: THE REQUEST DIRECTION ALONE\n'
      '   requestFloor is `data.len() as i64`: O(1). These rows are the\n'
      '   request copies plus the crossing and nothing else, which is what\n'
      '   makes them the instrument every request-path change is measured\n'
      '   against. They are NOT a throughput claim and NOT comparable to\n'
      '   section B, whose fixtures all do real work on the payload.',
  'A':
      'A. ROUND-TRIP FLOOR — axis: BRIDGE OVERHEAD\n'
      '   Payload is ~0, the Rust body is trivial. This is the cost of\n'
      '   CROSSING, not of doing anything. Do not read it as throughput.',
  'A2':
      'A2. PIPELINED — axis: BRIDGE OVERHEAD (async/actor, amortized)\n'
      '   64 calls in flight, awaited together, reported per call. This is\n'
      '   the async number comparable to the sync rows in A; the serial rows\n'
      '   in A are dominated by per-call thread wake + event-loop turn.',
  'B':
      'B. PAYLOAD SWEEP — axis: CODEC + TRANSPORT\n'
      '   Payload buffers are built ONCE, outside the timed region (safe:\n'
      '   the writer copies), so small rows are flattered by a cache-hot\n'
      '   source. Response allocation stays INSIDE the timed region, because\n'
      '   that is a real cost of every call.\n'
      '   Copy budget, traced in code (not inferred from these timings):\n'
      '     request  2 full payload copies for an OWNED parameter (Vec<u8>,\n'
      '              String, Vec<f64>) — the caller\'s Dart-heap source into\n'
      '              the transport\'s FFI block, and ByteReader::read_bytes\n'
      '              -> to_vec. The second is the price of the signature, not\n'
      '              of the bridge: an owned Vec must own its allocation. The\n'
      '              first IS the crossing.\n'
      '              A SYNC `&[u8]`/`&str` parameter pays only 1: the arm\n'
      '              decodes with read_bytes_borrowed() and the body reads\n'
      '              the transport\'s own buffer. Pool and actor arms stay\n'
      '              owned — they move their decoded values across a thread.\n'
      '              There is NO staging block on any path any more. The\n'
      '              generated encoder writes straight into the block the\n'
      '              call is about to hand Rust (BinaryWriter.external), on\n'
      '              sync and async alike, and every block is `malloc` —\n'
      '              nothing is zero-filled, because every byte read back out\n'
      '              was written by Rust or by us.\n'
      '              Two earlier versions of this note were wrong in opposite\n'
      '              directions: one put a memset on the sync path, which is\n'
      '              the one path that never had it; the next kept the\n'
      '              staging block after the request-path work deleted it.\n'
      '     response 2 more — the response ByteWriter, and Uint8List.fromList\n'
      '              back onto the Dart heap. The envelope status byte no\n'
      '              longer costs a third: the writer opens with its frame\n'
      '              already written (FramedWriter), so the body encodes into\n'
      '              its final wire position.\n'
      '     actor    +1 request copy — frustrate_actor_call does\n'
      '              request_slice(..).to_vec() so the closure can own the\n'
      '              request across the hop to the executor thread.\n'
      '   Because sumBytes returns an i64, sumBytes-vs-echoBytes at the same\n'
      '   size is a clean read on the RESPONSE half. For the request half,\n'
      '   read section B0: sumBytes cannot serve, because its body is an\n'
      '   O(n) widening sum and a request-side change cannot move that part\n'
      '   of the number.',
  'C':
      'C. WORK — axis: RUST CPU WORK, *NOT* BRIDGE OVERHEAD\n'
      '   The bridge is noise inside these numbers. Present only so section A\n'
      '   is not mistaken for this.',
};
