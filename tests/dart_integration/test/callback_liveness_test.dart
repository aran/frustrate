/// A blocking `DartFunction::call` is bounded in **liveness**, not duration
/// (runtime/rust/src/post.rs `handle_exit`).
///
/// The hazard this pins is a *hang*, harder to pin than the abort
/// `isolate_death_test` pins. `DartFunction::call` posts an
/// invocation and blocks its pool worker until Dart answers. A *refused* post
/// fails immediately, and a refused post to the same isolate later sweeps every
/// outstanding waiter — but an isolate that **accepts** the invocation and then
/// dies with it still queued leaves nobody to sweep. `Isolate.exit`, a Flutter
/// hot restart, and an uncaught error in a spawned isolate all produce exactly
/// that, and if nothing ever posts to the dead isolate again the worker parks
/// for the life of the process. The native pool is fixed at whatever width it
/// was built at — `available_parallelism()` unless an embedder declared one —
/// with no replenishment, so each occurrence permanently narrows it.
///
/// The fix is that the isolate reports its own death. It registers
/// `Isolate.current.addOnExitListener` against a native port Rust owns, so the
/// VM pushes the notice (~150 µs) and the sweep runs without anyone having to
/// post. That replaced an earlier polling probe, whose backoff this file's
/// timings were once sized against. So these tests come in matched pairs — one
/// that a *dead* isolate unparks the worker, and one that a *live* one does not
/// (without which "always report dead" would pass everything).
///
/// Native-only by construction. On web the blocking `call` never runs (the
/// consumer is the page, `post::wasm::deliver` is unconditionally `true`, and
/// the sweep is `#[cfg(not(target_family = "wasm"))]`).
///
/// Its own target, and for the same reason as `stream_shutdown_test`, only
/// sharper: pre-fix these do not fail an expectation, they park a pool worker
/// forever. Every await is bounded so the failure is legible, but a parked
/// worker also leaves the caller's `_pending` non-empty, which latches
/// `keepIsolateAlive` on the test isolate — so pre-fix this target may present
/// as a Bazel TIMEOUT rather than as the `fail(...)` message. Both are red;
/// neither may be allowed to mask a sibling suite.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:io';
import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// Bound for anything that should settle promptly. Generous: these bounds
/// exist to turn a hang into a report, not to measure anything.
const _bound = Duration(seconds: 10);

/// How long the slow-but-alive closure blocks for.
///
/// Nothing in the runtime has a cadence any more, so this is no longer sized
/// against one — it was once "longer than three probe intervals (250 + 500 +
/// 1000 ms)". It stays long, and stays matched to the async twin's constant, so
/// the two negatives remain comparable and so it would still outlive any
/// deadline a future fix was tempted to introduce.
const _slowClosure = Duration(milliseconds: 2600);

void main() {
  setUpAll(initBridge);

  // Retire every parked closure and wait for the ledger to go flat — the same
  // guard `bridge_test` runs, and load-bearing here rather than tidy. A parked
  // `DartFunction` is an open registration, and one owned by *this* isolate
  // pins it: the suite would pass, print "All tests
  // passed!", and then hang forever with nothing left to run. Under Bazel that
  // is an unexplained TIMEOUT with an empty log, because rules_dart's runner
  // shells out with `Process.runSync` and flushes only when the child exits.
  tearDown(() async {
    clearParkedFunctions();
    for (var i = 0; i < 200 && Frustrate.instance.openChannelCount != 0; i++) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'a parked callback outlived its test and pins this isolate. '
          'Still open: ${Frustrate.instance.openChannelLabels}',
    );
  });

  test(
    'a blocking call unparks when the isolate dies holding the invocation',
    () async {
      // Deterministic in both halves, the same way `park_sink` makes the
      // isolate-death pin deterministic: the closure itself reports that it is
      // running, so by the time we kill anything the invocation is provably
      // registered, delivered, accepted, and unanswered — the exact state no
      // later post would ever discover.
      final ready = ReceivePort();
      final invoked = ReceivePort();
      final exited = ReceivePort();
      final child = await Isolate.spawn(_parkAndNeverAnswer, [
        ready.sendPort,
        invoked.sendPort,
      ]);
      child.addOnExitListener(exited.sendPort);
      final slot = await ready.first.timeout(_bound) as int;
      ready.close();

      // Unawaited — it must still be in flight when the isolate dies. The
      // expectation is attached IMMEDIATELY, before the kill: the exit notice
      // lands in ~150 µs and can deliver the panic before this test's own
      // `exited` listener runs, and an errored future with no listener is an
      // unhandled async error that would fail this test for the wrong reason.
      final settled = expectLater(
        callParked(slot: slot, value: 1),
        throwsA(isA<BridgePanicException>()),
      );

      await invoked.first.timeout(_bound);
      invoked.close();
      expect(
        parkedCallState(),
        1,
        reason: 'the pool worker must be inside the blocking call',
      );

      // The one way an isolate ends while it still owes a callback response:
      // an open registration pins it, so it cannot simply exit. This is what
      // `Isolate.exit` and hot restart do.
      child.kill(priority: Isolate.immediate);
      await exited.first.timeout(_bound);
      exited.close();

      // Pre-fix this never completes: nothing posts to the dead isolate again,
      // so nothing sweeps the waiter and the worker parks for good.
      await settled.timeout(
        _bound,
        onTimeout: () => fail(
          'the pool worker never unblocked; '
          'parkedCallState()=${parkedCallState()} (1 = still parked inside '
          'DartFunction::call)',
        ),
      );
    },
  );

  test('a slow-but-alive closure still returns — liveness is not a deadline', () async {
    // The control, and what gives the pin teeth: without it, a fix that failed
    // every invocation on a timer would pass the test above. The closure is
    // registered in THIS isolate and nothing is killed, so the wait must run to
    // completion however long it takes.
    //
    // The spin blocks this isolate's event loop for `_slowClosure`, which is
    // also the shape that would expose any wait bounded in *duration* rather
    // than liveness — the isolate is alive but answering nothing.
    final slot = await parkFunction(
      f: (v) {
        final until = DateTime.now().add(_slowClosure);
        while (DateTime.now().isBefore(until)) {
          // Synchronous by contract: Rust is blocked on the return value, so
          // there is no yielding version of "take a long time".
        }
        return v * 2;
      },
    ).timeout(_bound);

    expect(
      await callParked(
        slot: slot,
        value: 21,
      ).timeout(const Duration(seconds: 30)),
      42,
      reason:
          'a slow-but-alive closure must still return: the wait ends on '
          'the owner being gone, it does not impose a deadline',
    );
    expect(
      parkedCallState(),
      2,
      reason: 'and the worker left the blocking call',
    );
  });

  test(
    'the pool recovers its full width after every waiter is orphaned',
    () async {
      // The severity claim, made observable: the cost of the defect was not one
      // lost call but a *permanently* narrowed pool. Park one blocking call per
      // worker, kill every owner, and then ask for ordinary async work — which
      // needs a worker, and pre-fix would never get one again.
      //
      // Width from Rust (`poolWidth`), never `Platform.numberOfProcessors`: the
      // pool sizes itself from `available_parallelism()`, which a cgroup quota
      // clamps below the processor count under Bazel.
      final width = poolWidth();
      expect(width, greaterThan(0));

      // Register every closure BEFORE occupying any worker. `parkFunction` is
      // itself a pool call, so interleaving would deadlock the setup on a pool
      // this test is in the middle of filling.
      final children = <Isolate>[];
      final slots = <int>[];
      final invoked = ReceivePort();
      final allInvoked = _counter(invoked, width);
      final exits = ReceivePort();
      final allExited = _counter(exits, width);
      for (var i = 0; i < width; i++) {
        final ready = ReceivePort();
        final child = await Isolate.spawn(_parkAndNeverAnswer, [
          ready.sendPort,
          invoked.sendPort,
        ]);
        child.addOnExitListener(exits.sendPort);
        children.add(child);
        slots.add(await ready.first.timeout(_bound) as int);
        ready.close();
      }

      final parked = [
        for (final slot in slots)
          expectLater(
            callParked(slot: slot, value: 1),
            throwsA(isA<BridgePanicException>()),
          ),
      ];
      await allInvoked.timeout(_bound);

      for (final child in children) {
        child.kill(priority: Isolate.immediate);
      }
      await allExited.timeout(_bound);
      await Future.wait(parked).timeout(
        const Duration(seconds: 30),
        onTimeout: () => fail(
          '$width blocking call(s) never unblocked; the '
          'pool is still narrowed',
        ),
      );
      invoked.close();
      exits.close();

      // The actual claim: the pool is whole again.
      expect(
        await sumSquares(n: 4).timeout(_bound),
        30,
        reason: 'ordinary pool work must run again on every reclaimed worker',
      );
    },
  );
}

/// A future that completes once [port] has delivered [n] messages. Listening
/// starts now, so nothing sent before the await is missed.
Future<void> _counter(ReceivePort port, int n) {
  final done = Completer<void>();
  var seen = 0;
  port.listen((_) {
    if (++seen == n && !done.isCompleted) done.complete();
  });
  return done.future;
}

/// Registers a closure that reports it is running and then never answers, parks
/// it where the parent can invoke it, and waits to be killed.
///
/// It cannot simply return: the registration pins it. And the
/// closure cannot await — it is synchronous by contract, since Rust is blocked
/// on its return value — so "never answer" means "never return".
Future<void> _parkAndNeverAnswer(List<SendPort> ports) async {
  final reply = ports[0];
  final invoked = ports[1];
  await initBridge();
  final slot = await parkFunction(
    f: (v) {
      invoked.send('invoked');
      // `sleep`, not a spin: this closure runs once per pool worker in the
      // width-recovery test, and saturating every core with busy-waiting
      // isolates starves the parent so badly that even its timeouts stop
      // firing — a test that cannot report its own failure. Blocking in short
      // sleeps costs no CPU and still honours `Isolate.immediate`, which is
      // taken at the loop's back edge (measured: the exit listener fires
      // within milliseconds).
      while (true) {
        sleep(const Duration(milliseconds: 5));
      }
    },
  );
  reply.send(slot);
  await Completer<void>().future;
}
