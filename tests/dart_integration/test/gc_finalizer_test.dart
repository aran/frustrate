/// Does the GC finalizer actually run?
///
/// Every other handle test disposes explicitly. This one never does: it
/// abandons a `LiveProbe` and asks whether the VM eventually reclaims the Rust
/// object behind it through `frustrate_finalize_LiveProbe`.
///
/// The answer decides what a channel-holding handle collected without
/// `dispose()` can be made to do: `stream::finalize_scope` ends such a channel
/// with a `LeakedChannelError` naming the holder type and the opening member,
/// which is only worth anything if the finalizer is reached at all.
///
/// **What a failure here means.** Dart guarantees only that a finalizer *may*
/// run, never that it will. A red result is therefore not proof that the path
/// is dead — only that this much allocation pressure did not reach it, which is
/// itself the useful signal: a diagnostic that needs this much prodding is not
/// one a developer would ever see.
///
/// VM-only: `NativeFinalizer` is a `dart:ffi` type and the whole question is
/// native-transport-shaped. Its own target because GC timing is the one thing
/// in this suite that is genuinely not deterministic, and it must not be able
/// to destabilize suites that are.
@TestOn('vm')
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// Allocate a probe and abandon it — no `dispose()`, no returned reference.
///
/// Its own function, never inlined: a local in the test body can stay live on
/// the frame for the rest of the method, which would keep the handle reachable
/// and make a red result meaningless. `isDisposed` is read only to use the
/// variable; the handle is unreachable the moment this returns.
@pragma('vm:never-inline')
void _abandonProbe() {
  final p = LiveProbe.new_();
  if (p.isDisposed) throw StateError('a fresh probe cannot be disposed');
}

/// Open a channel against a fresh `TextDoc` and abandon the handle — a stream
/// when [patches] is given, otherwise the closure flavour, which takes the
/// other delivery path (`_functions`). Same never-inlined discipline as
/// [_abandonProbe]: the handle must be unreachable when this returns.
@pragma('vm:never-inline')
void _abandonWatchingDoc(StreamController<TextPatch>? patches) {
  final doc = TextDoc.new_();
  if (patches != null) {
    doc.watch(sink: patches);
  } else {
    doc.onChange(cb: (_) {});
  }
  if (doc.isDisposed) throw StateError('a fresh doc cannot be disposed');
}

/// Apply allocation pressure until [reclaimed] or [limit] elapses; returns
/// whether it happened. Reports the elapsed time so a pass still says how hard
/// the VM had to be pushed — a pass at 20ms and a pass at 8s mean different
/// things for whether anyone would see a finalizer-based diagnostic.
Future<Duration?> _pressureUntil(
  bool Function() reclaimed, {
  Duration limit = const Duration(seconds: 10),
}) async {
  final sw = Stopwatch()..start();
  var sink = 0;
  while (sw.elapsed < limit) {
    // Short-lived multi-page allocations promote a new-space scavenge; the
    // running sum keeps the optimizer from eliding them.
    for (var i = 0; i < 32; i++) {
      final junk = List<int>.filled(1 << 14, i);
      sink += junk[junk.length - 1];
    }
    // Yield: NativeFinalizer callbacks are delivered on the message loop, so a
    // tight synchronous loop could allocate forever without ever running one.
    await Future<void>.delayed(Duration.zero);
    if (reclaimed()) return sw.elapsed;
  }
  expect(
    sink,
    isNonZero,
    reason: 'the pressure loop must not be optimized out',
  );
  return null;
}

/// Leak reports for a mirror with nowhere to put an error go to the zone the
/// transport was installed in (stream_router.dart). Installing inside a guarded
/// zone is therefore both how the suite observes them and the documented way an
/// app handles them — without it, an unhandled `LeakedChannelError` would abort
/// the test isolate.
final List<Object> zoneErrors = [];

Future<void> _initGuarded() {
  final ready = Completer<void>();
  runZonedGuarded(
    () async {
      await initBridge();
      ready.complete();
    },
    (e, st) {
      if (!ready.isCompleted) {
        ready.completeError(e, st);
        return;
      }
      zoneErrors.add(e);
    },
  );
  return ready.future;
}

void main() {
  setUpAll(_initGuarded);
  setUp(zoneErrors.clear);

  test('an abandoned handle is reclaimed through the GC finalizer', () async {
    final baseline = liveProbeCount();

    _abandonProbe();
    expect(
      liveProbeCount(),
      baseline + 1,
      reason: 'the Rust object must outlive the call that made it',
    );

    final took = await _pressureUntil(() => liveProbeCount() == baseline);
    expect(
      took,
      isNotNull,
      reason:
          'the VM never ran frustrate_finalize_LiveProbe under 10s of '
          'allocation pressure — the finalizer path may be effectively '
          'unreachable, and anything built on it would not fire in '
          'practice either',
    );
    printOnFailure('reclaimed after $took');
  }, timeout: const Timeout(Duration(seconds: 30)));

  test('many abandoned handles are all reclaimed', () async {
    // One probe reaching its finalizer could be luck. A batch says whether
    // reclamation is the rule, and whether it is all-or-nothing: a partial
    // sweep would mean a leak diagnostic fires for some handles and not
    // others, which is worse than one that never fires at all.
    final baseline = liveProbeCount();
    for (var i = 0; i < 50; i++) {
      _abandonProbe();
    }
    expect(liveProbeCount(), baseline + 50);

    final took = await _pressureUntil(() => liveProbeCount() == baseline);
    expect(
      took,
      isNotNull,
      reason:
          'reclaimed only ${liveProbeCount() - baseline} of 50 probes; '
          'partial reclamation makes any finalizer-based diagnostic '
          'nondeterministic per handle',
    );
    printOnFailure('50 probes reclaimed after $took');
  }, timeout: const Timeout(Duration(seconds: 30)));

  test('an abandoned stream terminates with an attributable leak error', () async {
    // The point of the whole mechanism: the consumer is told, by name, rather
    // than waiting forever on a producer that no longer exists.
    final patches = StreamController<TextPatch>();
    Object? terminal;
    final events = <String>[];
    patches.stream.listen(
      (_) {},
      onError: (Object e) {
        terminal = e;
        events.add('error');
      },
      onDone: () => events.add('done'),
    );

    final baseline = Frustrate.instance.openChannelCount;
    _abandonWatchingDoc(patches);
    expect(Frustrate.instance.openChannelCount, baseline + 1);
    expect(Frustrate.instance.openChannelLabels, contains('TextDoc.watch'));

    expect(
      await _pressureUntil(() => terminal != null),
      isNotNull,
      reason: 'the leaked channel must report, not simply stop',
    );

    expect(terminal, isA<LeakedChannelError>());
    final message = (terminal! as LeakedChannelError).message;
    expect(
      message,
      contains('TextDoc.watch'),
      reason: 'the binding half: which call opened the channel',
    );
    expect(
      message,
      contains('TextDoc'),
      reason: 'the Rust half: whose collection ended it',
    );
    expect(
      message,
      contains('dispose()'),
      reason: 'a contract violation names the remedy',
    );

    // An error terminal is still a terminal: the registration retires, which
    // is what lets a running isolate stop being pinned by the leak.
    expect(Frustrate.instance.openChannelCount, baseline);
    // The close is correct — a terminal terminates — but it must arrive *after*
    // the error. A bare 'done' is what this whole change exists to prevent.
    expect(events, ['error', 'done']);
  }, timeout: const Timeout(Duration(seconds: 30)));

  test('an abandoned closure mirror reports to the install zone', () async {
    // A closure has nowhere to put a terminal, so the report goes to the zone
    // instead of to a handler. That it is *catchable* there is the point: the
    // same error in the root zone would abort the isolate at whatever moment
    // the collector happened to run.
    final baseline = Frustrate.instance.openChannelCount;
    _abandonWatchingDoc(null);
    expect(Frustrate.instance.openChannelCount, baseline + 1);
    expect(Frustrate.instance.openChannelLabels, contains('TextDoc.on_change'));

    expect(
      await _pressureUntil(() => zoneErrors.isNotEmpty),
      isNotNull,
      reason: 'a leaked closure registration must report, not retire quietly',
    );
    expect(zoneErrors.single, isA<LeakedChannelError>());
    expect(
      (zoneErrors.single as LeakedChannelError).message,
      contains('TextDoc.on_change'),
    );
    expect(
      Frustrate.instance.openChannelCount,
      baseline,
      reason: 'the report is a terminal — it must retire the registration',
    );
  }, timeout: const Timeout(Duration(seconds: 30)));

  test(
    'a cancelled stream whose holder is later collected stays quiet',
    () async {
      // The gate that keeps this from crying wolf. Cancelling retires the
      // channel; collecting the holder afterwards reclaims memory and nothing
      // more. Reporting here would punish correct code for using the GC.
      final patches = StreamController<TextPatch>();
      Object? terminal;
      final sub = patches.stream.listen(
        (_) {},
        onError: (Object e) {
          terminal = e;
        },
      );

      final baseline = Frustrate.instance.openChannelCount;
      _abandonWatchingDoc(patches);
      await sub.cancel();
      expect(
        Frustrate.instance.openChannelCount,
        baseline,
        reason: 'cancel retires the registration on its own',
      );

      // Give the collector every chance to run the finalizer anyway.
      await _pressureUntil(
        () => terminal != null,
        limit: const Duration(seconds: 2),
      );
      expect(
        terminal,
        isNull,
        reason: 'a channel the consumer already closed was not leaked',
      );
    },
    timeout: const Timeout(Duration(seconds: 30)),
  );

  test('an explicit dispose() reclaims without waiting for the GC', () async {
    // The control. If this failed, the probe fixture itself would be broken and
    // the two tests above would be measuring nothing.
    final baseline = liveProbeCount();
    final p = LiveProbe.new_();
    expect(liveProbeCount(), baseline + 1);
    p.dispose();
    expect(
      liveProbeCount(),
      baseline,
      reason: 'dispose() drops the Rust object synchronously',
    );
  });
}
