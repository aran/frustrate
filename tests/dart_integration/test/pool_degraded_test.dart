/// A threaded-wasm pool that loses a worker it cannot replace keeps running,
/// and says so catchably.
///
/// The scenario is real rather than contrived: a panic retires a worker and
/// the runtime replenishes immediately — width is an invariant across traps —
/// but a deploy that moved the glue asset means the replacement never loads.
/// `_workerScriptUrl()` is read per spawn, so pointing `$frustrateGlueUrl` at
/// a missing file reproduces exactly that, and only for the *next* worker.
///
/// What this pins is the decision not to treat any failed spawn as fatal:
/// three workers can still drain the queue, so the calls in flight must
/// complete rather than be rejected. The seam's sequencing is unit-tested on
/// the VM (`runtime/dart/test/pool_workers_test.dart`); this is the end-to-end
/// half — that the narrowed pool genuinely still runs work, on real threads.
///
/// Its own file because it deliberately breaks worker spawning for the rest of
/// the page.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:js_interop_unsafe';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'guarded_init.dart';

void main() {
  setUpAll(initBridgeGuarded);

  test(
    'a worker the pool cannot replace narrows it, catchably, not fatally',
    () async {
      if (!Frustrate.instance.asyncIsParallel) {
        markTestSkipped('single-threaded web: async inline, no pool');
        return;
      }
      final width = poolWidth();
      expect(
        width,
        greaterThan(1),
        reason: 'the whole point is a pool that survives losing one',
      );

      // Spawn the pool healthy and confirm nothing was reported.
      expect(await poolNthPrime(n: 100), 541);
      expect(zoneErrors, isEmpty);

      // Move the glue out from under the runtime. Read per spawn, so the live
      // workers are untouched and only the next one to be created is affected.
      globalContext.setProperty(
        r'$frustrateGlueUrl'.toJS,
        '../build/no-such-glue.js'.toJS,
      );

      // The panic retires one worker; the replenishment it triggers 404s.
      await expectLater(poolPanic(), throwsA(isA<BridgePanicException>()));
      await until(
        () => zoneErrors.isNotEmpty,
        'the replacement worker to fail to load',
      );

      expect(
        zoneErrors,
        hasLength(1),
        reason: 'one loss, one report — not one per surviving worker',
      );
      final reported = zoneErrors.single;
      expect(reported, isA<StateError>());
      expect((reported as StateError).message, contains('failed to start'));
      expect(reported.message, contains('${width - 1} worker(s)'));
      expect(
        reported.message,
        contains('no-progress loop'),
        reason: 'the message must say why it will not be retried',
      );

      // Still serving calls — the load-bearing half of "degrade, do not fail".
      expect(await poolNthPrime(n: 100), 541);

      // And genuinely narrowed, exactly. The probe returns true only if that
      // many workers ran it *simultaneously*, so this is a width measurement,
      // not a timing heuristic.
      rendezvousReset();
      expect(
        await Future.wait(
          List.generate(
            width - 1,
            (_) => poolRendezvous(width: width - 1, maxWaitMs: 10000),
          ),
        ),
        everyElement(isTrue),
      );

      rendezvousReset();
      expect(
        await Future.wait(
          List.generate(
            width,
            (_) => poolRendezvous(width: width, maxWaitMs: 2000),
          ),
        ),
        contains(false),
        reason: 'the pool must no longer muster its original width',
      );

      expect(
        poolWidth(),
        width,
        reason:
            'Rust cannot observe a spawn failure that happened in the '
            'embedder, so pool::width() still reports the constant — which is '
            'why nothing may treat it as the live count',
      );
    },
  );
}
