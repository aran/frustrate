/// An actor handle nobody disposed is still reclaimed.
///
/// Until this landed, an actor was the one opaque model with no GC backstop:
/// every other one carries a `NativeFinalizer` through `HandleDrop`
/// (runtime_native.dart), while a generated actor held a bare `ActorHost` and
/// an `int? _raw` and nothing watched it. So the model with the *most* to leak
/// — a dedicated OS thread, plus whatever the Rust object owns, measured in one
/// real application at a 16-worker tokio runtime and two bound UDP sockets —
/// leaked all of it on a single forgotten `dispose()`, with no diagnostic.
///
/// Three separate claims, because passing the first two while failing the third
/// is exactly the shape a plausible-but-wrong fix would take:
///
///   1. the executor is released — `actorHostCount()` returns to baseline;
///   2. a channel the actor still held ends as **leaked**, naming the type,
///      not as *closed* the way an orderly shutdown ends it;
///   3. disposing under the same collection pressure, with handles going
///      unreachable mid-dispose, stays clean — no spurious failure, no leak.
///
/// Claim 3 started life as something stronger and did not survive its own
/// negative check; the test says what it actually establishes, and what it
/// cannot. See its comment.
///
/// **What a failure here means.** Dart promises a finalizer *may* run, never
/// that it will. A red result on claims 1–2 is therefore not proof the path is
/// dead, only that this much pressure did not reach it — which is itself the
/// signal, since a backstop needing this much prodding is not one anybody would
/// benefit from. A red result on claim 3 is unambiguous: that is a real bug.
///
/// VM-only, and its own target, for the same reasons as `gc_finalizer_test`:
/// `NativeFinalizer` is a `dart:ffi` type, and GC timing is the one genuinely
/// nondeterministic thing in this suite.
@TestOn('vm')
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// Spawn a `Miner` and abandon it — no `dispose()`, no returned reference.
///
/// Its own never-inlined function, exactly as `gc_finalizer_test`'s
/// `_abandonProbe`: a local in the test body can stay live on the frame for the
/// rest of the method, which would keep the handle reachable and make a red
/// result meaningless. `sink` is supplied from *outside* for the same reason in
/// reverse — the consumer has to outlive the actor, or there is nothing left to
/// receive the terminal.
@pragma('vm:never-inline')
Future<void> _abandonMiner([StreamController<int>? sink]) async {
  final m = await Miner.new_(label: 'abandoned');
  if (sink != null) {
    // A channel the actor keeps past the call that opened it, so the reaper's
    // terminal is observable.
    await m.watch(sink: sink);
  }
  if (await m.label() != 'abandoned') throw StateError('unreachable');
}

/// Start `dispose()` and let go of the handle before it finishes.
///
/// Deliberately **not** `async`: an async body would await the returned future
/// and keep `m` — a `Finalizable`-typed local — pinned for the whole dispose,
/// which is exactly the protection this test needs to step outside of. Written
/// this way, `m`'s scope ends the moment the future is returned, so the last
/// Dart reference to the handle is gone while the drop call is still in flight.
@pragma('vm:never-inline')
Future<void> _disposeAndAbandon(Miner m) => m.dispose();

/// Apply allocation pressure until [reclaimed], or give up. Mirrors
/// `gc_finalizer_test._pressureUntil` — including reporting the elapsed time,
/// so a pass still says how hard the VM had to be pushed.
Future<Duration?> _pressureUntil(
  bool Function() reclaimed, {
  Duration limit = const Duration(seconds: 10),
}) async {
  final sw = Stopwatch()..start();
  var sink = 0;
  while (sw.elapsed < limit) {
    for (var i = 0; i < 32; i++) {
      final junk = List<int>.filled(1 << 14, i);
      sink += junk[junk.length - 1];
    }
    // NativeFinalizer callbacks are delivered on the message loop, so a tight
    // synchronous loop could allocate forever without ever running one.
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

/// Leak reports for a channel with nowhere to put an error go to the zone the
/// transport was installed in (stream_router.dart), so the suite has to install
/// inside a guarded one — the same arrangement `gc_finalizer_test` uses, and
/// the documented way an app handles them.
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

  test('an abandoned actor releases its executor', () async {
    final baseline = actorHostCount();

    await _abandonMiner();
    expect(
      actorHostCount(),
      baseline + 1,
      reason: 'the executor must outlive the call that spawned it',
    );

    final took = await _pressureUntil(() => actorHostCount() == baseline);
    expect(
      took,
      isNotNull,
      reason:
          'the VM never reaped the actor host under 10s of allocation '
          'pressure — the reaper may be effectively unreachable, in which '
          'case an undisposed actor still leaks a thread and its object',
    );
    printOnFailure('reaped after $took');
  }, timeout: const Timeout(Duration(seconds: 30)));

  test(
    'an abandoned actor ends its channel as leaked, not as closed',
    () async {
      // The half a host-count assertion cannot see. Reaping through a bare drop
      // would satisfy the test above and still be wrong here: the channel would
      // terminate normally, and an app would watch a stream end cleanly with no
      // hint that the actor behind it was abandoned rather than shut down.
      final sink = StreamController<int>();
      final events = <String>[];
      Object? terminal;
      sink.stream.listen(
        (_) {},
        onError: (Object e) {
          terminal = e;
          events.add('error');
        },
        onDone: () => events.add('done'),
      );

      final baseline = actorHostCount();
      await _abandonMiner(sink);

      final took = await _pressureUntil(() => terminal != null);
      expect(
        took,
        isNotNull,
        reason: 'a reaped actor holding an open channel must report it',
      );
      printOnFailure('reported after $took');

      expect(
        terminal,
        isA<LeakedChannelError>(),
        reason:
            'the channel ended without a LeakedChannelError — a reap that '
            'closes channels normally is indistinguishable to an app from an '
            'orderly shutdown, which is the diagnostic this exists to give',
      );
      final message = (terminal! as LeakedChannelError).message;
      expect(
        message,
        contains('Miner.watch'),
        reason: 'the binding half: which call opened the channel',
      );
      expect(
        message,
        contains('Miner'),
        reason: 'the Rust half: whose reap ended it',
      );

      // An error terminal is still a terminal, and it must arrive *before* the
      // close — a bare 'done' is exactly what this reports instead of.
      expect(events, ['error', 'done']);
      expect(
        actorHostCount(),
        baseline,
        reason: 'reporting the leak must not skip releasing the executor',
      );
    },
    timeout: const Timeout(Duration(seconds: 30)),
  );

  test('disposing while the collector runs stays clean', () async {
    // Claim 3, and the honest version of it. The draft asserted that the
    // synchronous detach in dispose() prevents a mid-dispose reap, and the
    // negative check refuted that twice: moving the detach after the await
    // left this green even with every Dart reference to the handle dropped
    // mid-flight. The reason is structural — `_issueAsync` is not `async`, so
    // the drop call is on the executor's FIFO before dispose can suspend, and
    // any later reap queues behind it and finds the slot already disarmed.
    // The detach releases what the finalizer retains; it is not an ordering
    // guard, and nothing here pretends to isolate it.
    //
    // What this does pin is the interaction end to end: 40 actors disposed
    // with their handles going unreachable mid-dispose, under collection
    // pressure, must release every executor and report no leak. A reaper that
    // double-terminated, or fired against a disposed host, or left a channel
    // open, would land here.
    //
    // The handle is never bound to a local on purpose: `Miner` includes
    // `Finalizable`, so `final m = ...; await m.dispose();` pins it for the
    // whole scope and could not go unreachable at all.
    final baseline = actorHostCount();
    final inFlight = <Future<void>>[];
    for (var i = 0; i < 40; i++) {
      inFlight.add(_disposeAndAbandon(await Miner.new_(label: 'disposed-$i')));
    }
    // Collect *while* those disposes are mid-flight — the window the detach
    // closes is between `_raw = null` and the drop call landing.
    final pressure = _pressureUntil(
      () => false,
      limit: const Duration(milliseconds: 800),
    );

    // The assertion is that none of these throw: a reap landing anywhere it
    // should not would surface as a dead-host StateError out of `_host.call`.
    await Future.wait(inFlight);
    await pressure;

    await _pressureUntil(() => actorHostCount() == baseline);
    expect(
      actorHostCount(),
      baseline,
      reason: 'every disposed actor must have released its executor',
    );
    expect(
      zoneErrors,
      isEmpty,
      reason: 'an orderly dispose must report no leak: $zoneErrors',
    );
  }, timeout: const Timeout(Duration(seconds: 60)));
}
