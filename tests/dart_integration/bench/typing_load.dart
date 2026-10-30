/// The typing-load scenario: can one actor be a notes app's core on the typing
/// path? Shared by both release drivers, which pass `--typing`:
///
///     bazel run //tests/dart_integration:native_bench -- --typing
///     bazel run //tests/dart_integration:web_bench -- --typing
///
/// Imports neither `dart:io` nor `dart:js_interop`, so the same code runs in the
/// native binary and in the -O2 page. Every time here is a `Stopwatch` reading,
/// which on dart2wasm is `performance.now()` (5 µs ticks under the page's
/// COOP/COEP), and every handler time is the actor's own clock, read inside the
/// handler (`NoteCore` in tests/test_api/src/api.rs).
///
/// One `NoteCore` actor holds the note. Two phases, both with a typist sending
/// one keystroke every 1/keyHz s on an absolute schedule, each an actor call
/// that edits at the caret and replies with a confirmation:
///
///   * idle: the typist alone;
///   * load: the typist, a remote peer sending `remoteHz` byte-encoded changes
///     a second, and a background job that wants `jobUs` of CPU, run back to
///     back so the core is never idle.
///
/// The job yields in one of two ways. By default it is driven from this
/// isolate: each slice is one `backgroundSlice` call that runs at most
/// `sliceUs`, and the next is sent when its reply arrives, so a keystroke that
/// arrives during a slice runs before the next slice, which has not been sent
/// yet, and the core idles for one round trip between slices. With
/// `--typing-self-post` the core slices itself: each slice posts the next with
/// `Addr::post`, which queues it behind every message already waiting, so a
/// keystroke that arrives during a slice still runs before the next one, and
/// the core is never idle.
///
/// Every edit is published to this isolate as a `NoteChange` (the edit, not
/// the note); the run checks that each one arrived.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';

import 'typing_config.dart';

export 'typing_config.dart';

/// Microsecond samples of one quantity.
class Samples {
  Samples(this.name);
  final String name;
  final List<double> us = [];

  int get n => us.length;
  double q(double p) {
    if (us.isEmpty) return double.nan;
    final s = [...us]..sort();
    return s[(p * s.length).ceil().clamp(1, s.length) - 1];
  }

  double get max => us.isEmpty ? double.nan : us.reduce(math.max);

  Map<String, Object?> toJson() => {
    'n': n,
    'p50_us': _r(q(0.5)),
    'p99_us': _r(q(0.99)),
    'max_us': _r(max),
  };
}

double _r(double v) => v.isNaN ? v : (v * 10).roundToDouble() / 10;

class TypingLoadResult {
  TypingLoadResult(this.config);
  final TypingLoadConfig config;

  final idleRoundTrip = Samples('idle keystroke round trip');
  final idleKeyHandler = Samples('idle keystroke handler');
  final loadRoundTrip = Samples('load keystroke round trip');
  final loadHandler = Samples('load handler, every message');
  final loadKeyHandler = Samples('load keystroke handler');
  final loadRemoteHandler = Samples('load remote-change handler');
  final loadSliceHandler = Samples('load background-slice handler');
  final remoteRoundTrip = Samples('load remote-change round trip');

  /// How late this isolate's typist timer fired against its schedule: the
  /// main isolate's own congestion, which a round trip includes.
  final timerLag = Samples('typist timer lag');

  int edits = 0;
  int changesPublished = 0;
  int jobsDone = 0;
  int slices = 0;

  Map<String, Object?> toJson() => {
    'config': config.toJson(),
    for (final s in [
      idleRoundTrip,
      idleKeyHandler,
      loadRoundTrip,
      loadHandler,
      loadKeyHandler,
      loadRemoteHandler,
      loadSliceHandler,
      remoteRoundTrip,
      timerLag,
    ])
      s.name: s.toJson(),
    'edits': edits,
    'changes_published': changesPublished,
    'jobs_done': jobsDone,
    'slices': slices,
  };

  /// The report. [budgets] maps a row key, (a) (b) (c), to its budget in ms.
  String render({
    required String title,
    required List<String> banner,
    required Map<String, double> budgets,
  }) {
    final b = StringBuffer()
      ..writeln('')
      ..writeln(title)
      ..writeln('=' * 78);
    banner.forEach(b.writeln);
    final c = config;
    b
      ..writeln(
        'scenario    one NoteCore actor, ${c.noteChars}-char note in a String; '
        'keystroke = insert at caret',
      )
      ..writeln(
        '            (every 10th a backspace); ${c.keyHz} keys/s; idle '
        '${c.idleSeconds} s, then load ${c.loadSeconds} s:',
      )
      ..writeln(
        '            + ${c.remoteHz} remote changes/s (bytes in) + a '
        '${c.jobUs ~/ 1000} ms job in ${c.sliceUs / 1000} ms slices, back to '
        'back,',
      )
      ..writeln(
        c.selfPost
            ? '            each slice posted by the last with Addr::post.'
            : '            each slice one call sent by this isolate when the '
                  'last replied.',
      )
      ..writeln(
        'clocks      round trips: Stopwatch on this isolate. handlers: the '
        'actor\'s own',
      )
      ..writeln(
        '            clock inside the handler body (excludes request decode, '
        'reply encode).',
      )
      ..writeln('')
      ..writeln(
        '  ${'quantity'.padRight(44)}${'n'.padLeft(7)}${'p50 ms'.padLeft(9)}'
        '${'p99 ms'.padLeft(9)}${'max ms'.padLeft(9)}${'budget'.padLeft(8)}',
      );
    String ms(double us) => (us / 1000).toStringAsFixed(3).padLeft(9);
    void row(String label, Samples s, [String? key]) {
      final budget = key == null ? '' : '${budgets[key]} ms';
      b.writeln(
        '  ${label.padRight(44)}${s.n.toString().padLeft(7)}${ms(s.q(0.5))}'
        '${ms(s.q(0.99))}${ms(s.max)}${budget.padLeft(8)}',
      );
    }

    row('(a) keystroke round trip, core idle', idleRoundTrip, 'a');
    row('(b) handler duration, every message, load', loadHandler, 'b');
    row('(c) keystroke round trip, full load', loadRoundTrip, 'c');
    b.writeln('');
    row('    keystroke handler, idle', idleKeyHandler);
    row('    keystroke handler, load', loadKeyHandler);
    row('    remote-change handler, load', loadRemoteHandler);
    row('    background-slice handler, load', loadSliceHandler);
    row('    remote-change round trip, load', remoteRoundTrip);
    row('    typist timer lag (this isolate), both', timerLag);
    b
      ..writeln('')
      ..writeln(
        'checks      $edits edits, $changesPublished NoteChange items '
        'received; $slices slices, $jobsDone jobs finished',
      )
      ..writeln(
        '            (b) budget applies to max; p50/p99 are over all '
        'handlers, slices dominating.',
      );
    return b.toString();
  }
}

/// Run both phases against a fresh `NoteCore`. [log] gets one line per phase.
Future<TypingLoadResult> runTypingLoad(
  TypingLoadConfig c, {
  void Function(String) log = _noLog,
}) async {
  final result = TypingLoadResult(c);
  final core = await NoteCore.new_(chars: c.noteChars, jobUs: c.jobUs);
  final changes = StreamController<NoteChange>();
  var lastSeq = 0;
  var seqGap = false;
  final sub = changes.stream.listen((ch) {
    if (ch.seq != lastSeq + 1) seqGap = true;
    lastSeq = ch.seq;
    result.changesPublished++;
  });
  await core.publish(sink: changes);

  const typed = 'the quick note grows by one key at a time ';
  var keyIndex = 0;
  String nextKey() {
    final i = keyIndex++;
    return i % 10 == 9 ? '' : typed[i % typed.length];
  }

  final clock = Stopwatch()..start();
  final pending = <Future<void>>[];

  // Warm-up, untimed: every handler and both directions of the channel.
  for (var i = 0; i < 200; i++) {
    await core.key(text: nextKey());
  }
  for (var i = 0; i < 10; i++) {
    await core.remote(change: _remoteChange(i));
    await core.backgroundSlice(sliceUs: c.sliceUs);
  }
  result.edits = 210;

  // A periodic sender on an absolute schedule, so a late tick does not push
  // every later one back.
  Future<void> every(
    int hz,
    Duration length,
    void Function(int k) send, {
    Samples? lag,
  }) {
    final done = Completer<void>();
    final periodUs = 1000000 ~/ hz;
    final startUs = clock.elapsedMicroseconds;
    final endUs = startUs + length.inMicroseconds;
    var k = 0;
    void tick() {
      final now = clock.elapsedMicroseconds;
      if (now >= endUs) {
        done.complete();
        return;
      }
      lag?.us.add((now - (startUs + k * periodUs)).toDouble());
      send(k++);
      final nextUs = startUs + k * periodUs;
      Timer(
        Duration(microseconds: math.max(0, nextUs - clock.elapsedMicroseconds)),
        tick,
      );
    }

    tick();
    return done.future;
  }

  void typeOne(Samples roundTrip, Samples handler, Samples? all) {
    final t0 = clock.elapsedMicroseconds;
    pending.add(
      core.key(text: nextKey()).then((ack) {
        roundTrip.us.add((clock.elapsedMicroseconds - t0).toDouble());
        handler.us.add(ack.handlerNs / 1000);
        all?.us.add(ack.handlerNs / 1000);
        result.edits++;
      }),
    );
  }

  // -- idle ------------------------------------------------------------------
  await every(
    c.keyHz,
    Duration(seconds: c.idleSeconds),
    (_) => typeOne(result.idleRoundTrip, result.idleKeyHandler, null),
    lag: result.timerLag,
  );
  await Future.wait(pending);
  pending.clear();
  log(
    'typing: idle done, ${result.idleRoundTrip.n} keystrokes, '
    'p99 ${(result.idleRoundTrip.q(0.99) / 1000).toStringAsFixed(3)} ms',
  );

  // -- load ------------------------------------------------------------------
  var loading = true;
  Future<void> background() async {
    while (loading) {
      final ack = await core.backgroundSlice(sliceUs: c.sliceUs);
      result.loadHandler.us.add(ack.handlerNs / 1000);
      result.loadSliceHandler.us.add(ack.handlerNs / 1000);
      result.jobsDone = ack.jobsDone;
      result.slices++;
    }
  }

  if (c.selfPost) await core.startBackground(sliceUs: c.sliceUs);
  final job = c.selfPost ? Future<void>.value() : background();
  await Future.wait([
    every(
      c.keyHz,
      Duration(seconds: c.loadSeconds),
      (_) => typeOne(
        result.loadRoundTrip,
        result.loadKeyHandler,
        result.loadHandler,
      ),
      lag: result.timerLag,
    ),
    every(c.remoteHz, Duration(seconds: c.loadSeconds), (k) {
      final t0 = clock.elapsedMicroseconds;
      pending.add(
        core.remote(change: _remoteChange(k)).then((ack) {
          result.remoteRoundTrip.us.add(
            (clock.elapsedMicroseconds - t0).toDouble(),
          );
          result.loadRemoteHandler.us.add(ack.handlerNs / 1000);
          result.loadHandler.us.add(ack.handlerNs / 1000);
          result.edits++;
        }),
      );
    }),
  ]);
  loading = false;
  await job;
  if (c.selfPost) {
    final report = await core.stopBackground();
    for (final ns in report.handlerNs) {
      result.loadHandler.us.add(ns / 1000);
      result.loadSliceHandler.us.add(ns / 1000);
    }
    result.slices = report.handlerNs.length;
    result.jobsDone = report.jobsDone;
  }
  await Future.wait(pending);
  log(
    'typing: load done, ${result.loadRoundTrip.n} keystrokes, '
    '${result.slices} slices, p99 '
    '${(result.loadRoundTrip.q(0.99) / 1000).toStringAsFixed(3)} ms',
  );

  // Every edit's change must reach this isolate, in order.
  final deadline = clock.elapsedMicroseconds + 5000000;
  while (result.changesPublished < result.edits &&
      clock.elapsedMicroseconds < deadline) {
    await Future<void>.delayed(const Duration(milliseconds: 10));
  }
  await sub.cancel();
  await core.dispose();
  if (seqGap || result.changesPublished != result.edits) {
    throw StateError(
      'typing: ${result.edits} edits but ${result.changesPublished} published '
      'changes arrived (sequence gap: $seqGap)',
    );
  }
  return result;
}

void _noLog(String _) {}

/// A remote peer's change, as the wire bytes `NoteCore.remote` reads: replace
/// three bytes somewhere in the first 190 000 with a short word.
Uint8List _remoteChange(int k) {
  final text = utf8.encode('peer${k % 10} ');
  final b = ByteData(6 + text.length)
    ..setUint32(0, (k * 104729) % 190000, Endian.little)
    ..setUint16(4, 3, Endian.little);
  final out = b.buffer.asUint8List()..setRange(6, 6 + text.length, text);
  return out;
}

/// The budgets the scenario was set, in ms, per platform.
const Map<String, double> nativeBudgets = {'a': 1, 'b': 8, 'c': 10};
const Map<String, double> webBudgets = {'a': 4, 'b': 8, 'c': 12};
