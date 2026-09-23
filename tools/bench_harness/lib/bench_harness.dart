/// The platform-neutral half of the benchmark harness: calibration, batching,
/// the [Cell] statistic, and the table/NOTES renderer.
///
/// **Imports neither `dart:io` nor `dart:ffi`.** That is the whole point of the
/// file — it is compiled into a browser page by `bench/web_bench_main.dart` as
/// well as linked into the native `dart_binary` at `tool/native_bench.dart`.
/// Anything platform-specific (process spawning, file hashing, `stdout`) belongs
/// in the caller; [renderReport] returns a `String` rather than writing one.
///
/// Extracted from `tool/native_bench.dart`, which now imports it instead of
/// carrying a second copy. Two things had to change in the move and nothing
/// else did:
///
///   * `_sentinel` became the public [sentinel] — the measured bodies live in
///     the caller's library now, so a private field is unreachable. It has a
///     constant initializer, so there is no lazy-init guard on the access path
///     under either compiler.
///   * `_report` wrote to `stdout`; it is now [renderReport], which takes the
///     caller's prose (title, banner, section blurbs) and returns the rendered
///     table. The native driver passes its own strings, so its output is
///     unchanged apart from the numbers, which move every run anyway.
///
/// This file asserts nothing about absolute time, and neither does anything
/// that imports it.
library;

/// Batches per cell. Reported statistic is the **median** batch; p10/p90 are
/// reported for shape and are never a bar. `min` is also reported because for
/// an overhead floor it is the better estimator — scheduling noise is strictly
/// additive, so the fastest batch is the closest to the real cost.
const int batches = 7;

/// Wall time each batch aims for. The enemy is not clock resolution
/// (`Stopwatch` is nanosecond-granular on the VM) but the scheduler quantum, so
/// this is set in tens of milliseconds rather than at the clock's precision.
///
/// On dart2wasm `Stopwatch` is `1000 * performance.now()`, so the real
/// resolution is the browser's: 5 us in a cross-origin-isolated page, 100 us
/// otherwise. Both are noise against a 50 ms batch — but the page must measure
/// and report which one it got rather than assume (`bench/web_bench_main.dart`).
const int targetBatchUs = 50000;

/// Anti-DCE accumulator. Every measured body folds something cheap and
/// result-dependent into this, and the driver asserts it at the end.
/// Deliberately *not* a checksum over the payload: at 1 MiB, hashing every byte
/// costs more than the call being measured, and the mitigation becomes the
/// measurement.
int sentinel = 0;

/// One measured cell.
class Cell {
  Cell(
    this.section,
    this.label,
    this.axis,
    this.bytes,
    this.iters,
    this.samplesUs, {
    this.note,
  });

  final String section;
  final String label;

  /// What the number *is*. `overhead` = the cost of crossing the bridge, where
  /// the Rust body is trivial. `work` = the cost of the Rust body, where the
  /// bridge is noise. Reporting one as the other is the easiest way to publish
  /// a misleading benchmark, so every cell carries this explicitly.
  final String axis;

  /// Payload bytes per call (request side), or null where not applicable.
  final int? bytes;
  final int iters;
  final List<double> samplesUs;
  final String? note;

  List<double> get _sorted => [...samplesUs]..sort();
  double _q(double q) {
    final s = _sorted;
    return s[(q * s.length).floor().clamp(0, s.length - 1)];
  }

  double get medianUs => _q(0.5);
  double get minUs => _sorted.first;
  double get p10Us => _q(0.1);
  double get p90Us => _q(0.9);

  /// Only meaningful for the payload sweep.
  double? get mibPerSec => bytes == null || bytes == 0
      ? null
      : (bytes! / (1 << 20)) / (medianUs / 1e6);

  Map<String, Object?> toJson() => {
    'section': section,
    'label': label,
    'axis': axis,
    if (bytes != null) 'payload_bytes': bytes,
    'iters_per_batch': iters,
    'batches': samplesUs.length,
    'median_us': roundSig(medianUs),
    'min_us': roundSig(minUs),
    'p10_us': roundSig(p10Us),
    'p90_us': roundSig(p90Us),
    if (mibPerSec != null) 'mib_per_sec': roundSig(mibPerSec!),
    if (note != null) 'note': note,
  };
}

double roundSig(double v) =>
    double.parse(v.toStringAsPrecision(v.abs() < 1 ? 3 : 5));

int itersFor(double perCallUs) =>
    (targetBatchUs / (perCallUs <= 0 ? 0.001 : perCallUs)).ceil().clamp(
      1,
      20000000,
    );

/// Measure a synchronous body: calibrate, discard a full-size warmup batch,
/// then take [batches] timed batches.
///
/// The warmup batch is not a formality. The first call through any of these
/// paths pays lazy `lookupFunction` resolution, dylib page faults, and (for the
/// async/actor arms) the entire worker-pool or executor-thread spawn — all of
/// which would otherwise land inside a timed region. On wasm it also pays V8's
/// Liftoff->TurboFan tier-up, which is why the web driver additionally runs a
/// global pre-warm pass over *every* body before the first cell is timed: the
/// first cell would otherwise be measured against a colder shared code path
/// (the writer, the codec, the transport) than the last.
Cell measureSync(
  String section,
  String label,
  String axis,
  void Function() body, {
  int? bytes,
  String? note,
}) {
  final cal = Stopwatch()..start();
  var calls = 0;
  while (cal.elapsedMicroseconds < 5000) {
    body();
    calls++;
  }
  cal.stop();
  final iters = itersFor(cal.elapsedMicroseconds / calls);

  for (var i = 0; i < iters; i++) {
    body();
  }

  final samples = <double>[];
  for (var b = 0; b < batches; b++) {
    final sw = Stopwatch()..start();
    for (var i = 0; i < iters; i++) {
      body();
    }
    sw.stop();
    samples.add(sw.elapsedMicroseconds / iters);
  }
  return Cell(section, label, axis, bytes, iters, samples, note: note);
}

/// The async twin. `body` must await one full round trip.
Future<Cell> measureAsync(
  String section,
  String label,
  String axis,
  Future<void> Function() body, {
  int? bytes,
  String? note,
}) async {
  final cal = Stopwatch()..start();
  var calls = 0;
  while (cal.elapsedMicroseconds < 5000) {
    await body();
    calls++;
  }
  cal.stop();
  final iters = itersFor(cal.elapsedMicroseconds / calls);

  for (var i = 0; i < iters; i++) {
    await body();
  }

  final samples = <double>[];
  for (var b = 0; b < batches; b++) {
    final sw = Stopwatch()..start();
    for (var i = 0; i < iters; i++) {
      await body();
    }
    sw.stop();
    samples.add(sw.elapsedMicroseconds / iters);
  }
  return Cell(section, label, axis, bytes, iters, samples, note: note);
}

/// Pipelined throughput: [inFlight] calls issued back to back, then awaited
/// together. This is the number comparable to the sync column — a serial
/// `await` pays a thread wake plus a full event-loop turn *per call*, which is
/// scheduling latency, not bridge cost. Reported per call.
Future<Cell> measurePipelined(
  String section,
  String label,
  String axis,
  Future<void> Function() one,
  int inFlight, {
  String? note,
}) async {
  Future<void> wave() => Future.wait(List.generate(inFlight, (_) => one()));

  final cal = Stopwatch()..start();
  var waves = 0;
  while (cal.elapsedMicroseconds < 5000) {
    await wave();
    waves++;
  }
  cal.stop();
  final perCall = cal.elapsedMicroseconds / (waves * inFlight);
  final iters = (itersFor(perCall) / inFlight).ceil().clamp(1, 100000);

  for (var i = 0; i < iters; i++) {
    await wave();
  }

  final samples = <double>[];
  for (var b = 0; b < batches; b++) {
    final sw = Stopwatch()..start();
    for (var i = 0; i < iters; i++) {
      await wave();
    }
    sw.stop();
    samples.add(sw.elapsedMicroseconds / (iters * inFlight));
  }
  return Cell(section, label, axis, 0, iters * inFlight, samples, note: note);
}

/// Render the table. [banner] is the caller's provenance block (the lines
/// between the rule and the shared `method` block); [sections] maps a section
/// key to the blurb printed above its rows. Returns the report rather than
/// printing it, so a browser page can POST the same bytes a terminal prints.
String renderReport({
  required String title,
  required List<String> banner,
  required Map<String, String> sections,
  required List<Cell> cells,
}) {
  final b = StringBuffer();
  b.writeln('');
  b.writeln(title);
  b.writeln('=' * 78);
  for (final line in banner) {
    b.writeln(line);
  }
  b.writeln(
    'method      $batches timed batches per cell after one discarded '
    'warmup batch;',
  );
  b.writeln(
    '            batch size auto-scaled to ~'
    '${(targetBatchUs / 1000).round()} ms of wall time; median reported.',
  );
  b.writeln(
    '            No absolute-time assertion is made anywhere, here or '
    'in `bazel test`.',
  );
  b.writeln('');

  const headers = ['us/call', 'min', 'p10', 'p90', 'MiB/s', 'iters'];
  void row(Cell c) {
    final cols = [
      c.medianUs.toStringAsFixed(c.medianUs < 10 ? 3 : 1).padLeft(9),
      c.minUs.toStringAsFixed(c.minUs < 10 ? 3 : 1).padLeft(9),
      c.p10Us.toStringAsFixed(c.p10Us < 10 ? 3 : 1).padLeft(9),
      c.p90Us.toStringAsFixed(c.p90Us < 10 ? 3 : 1).padLeft(9),
      (c.mibPerSec?.toStringAsFixed(0) ?? '-').padLeft(7),
      c.iters.toString().padLeft(8),
    ];
    b.writeln('  ${c.label.padRight(52)}${cols.join()}');
  }

  for (final entry in sections.entries) {
    final rows = cells.where((c) => c.section == entry.key).toList();
    if (rows.isEmpty) continue;
    b.writeln(entry.value);
    b.writeln(
      '  ${'fixture'.padRight(52)}'
      '${headers.map((h) => h.padLeft(h == 'MiB/s' ? 7 : (h == 'iters' ? 8 : 9))).join()}',
    );
    for (final c in rows) {
      row(c);
    }
    b.writeln('');
  }

  final noted = cells.where((c) => c.note != null).toList();
  if (noted.isNotEmpty) {
    b.writeln('NOTES');
    final seen = <String>{};
    for (final c in noted) {
      if (!seen.add(c.note!)) continue;
      b.writeln('  * ${c.label}:');
      for (final line in wrapText(c.note!, 72)) {
        b.writeln('      $line');
      }
    }
    b.writeln('');
  }
  return b.toString();
}

List<String> wrapText(String s, int width) {
  final out = <String>[];
  var line = StringBuffer();
  for (final word in s.split(RegExp(r'\s+'))) {
    if (line.isNotEmpty && line.length + 1 + word.length > width) {
      out.add(line.toString());
      line = StringBuffer();
    }
    if (line.isNotEmpty) line.write(' ');
    line.write(word);
  }
  if (line.isNotEmpty) out.add(line.toString());
  return out;
}
