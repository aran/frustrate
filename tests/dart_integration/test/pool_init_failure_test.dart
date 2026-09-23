/// A threaded-wasm pool that never starts fails its calls attributably
/// instead of hanging them.
///
/// `worker.onerror` and the `initError` message both fire from a bare JS event
/// dispatch, so a `throw` there reaches nothing but the browser's
/// uncaught-error log while every issued future waits forever. The cost is a
/// hang, which is what this suite has to reproduce — an assertion that merely
/// checks the error *type* would pass against a build that still hangs.
///
/// The blast radius is the other half of the contract: with no pool worker
/// left, nothing can run off the main thread, but `callSync` runs inline on
/// the main instance and every actor owns its own worker. Both must keep
/// working.
///
/// Its own file because the Rust pool spawns exactly once per page: the queue
/// is a const-constructed `static QUEUE` and a `SPAWN_STARTED` `AtomicBool`
/// claims the one-time spawn (runtime/rust/src/pool.rs), so once any other
/// suite has triggered it here no later call will spawn again. Each `dart
/// test` suite gets its own page, which is what keeps that latch per-suite.
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
    'a pool that never starts rejects its calls and spares the rest',
    () async {
      if (!Frustrate.instance.asyncIsParallel) {
        markTestSkipped('single-threaded web: async inline, no pool');
        return;
      }
      // Break worker loading *after* init — `_installGlue` already ran and is
      // page-wide, while `_workerScriptUrl()` is re-read on every spawn. No
      // async call has been made yet, so the pool has not been created.
      globalContext.setProperty(
        r'$frustrateGlueUrl'.toJS,
        '../build/no-such-glue.js'.toJS,
      );

      // The first async call creates the pool. All four workers 404, and the
      // last one to fail is what makes it fatal.
      await expectLater(
        poolNthPrime(n: 100),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('every threaded-wasm pool worker failed to start'),
          ),
        ),
      );

      // A later call must not hang either: it rejects off the latch, without
      // re-entering wasm.
      await expectLater(poolNthPrime(n: 100), throwsA(isA<StateError>()));

      // Scoping. A sync call runs inline on the main instance and is unaffected
      // (a real `callSync` round trip through the module).
      expect(poolWidth(), 4);

      // And an actor still spawns and serves, because it owns its own worker
      // rather than drawing from the pool. Point the glue back at the embedded
      // copy first — empty means "unset" to `_servedGlueUrl`.
      globalContext.setProperty(r'$frustrateGlueUrl'.toJS, ''.toJS);
      final miner = await Miner.new_(label: 'after the pool died');
      expect(await miner.label(), 'after the pool died');
      expect(await miner.nthPrime(n: 100), 541);
      await miner.dispose();

      // One report on the way down: the first worker's loss was true when it
      // was made, the rest restate it, and the fatal supersedes them.
      expect(zoneErrors, hasLength(1));
      expect(zoneErrors.single, isA<StateError>());
    },
  );
}
