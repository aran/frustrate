/// A pool spawn that fails *before* the worker is created latches like any
/// other spawn failure.
///
/// `_spawnPoolWorkerInner` reserves the new thread's stack and TLS out of the
/// module's own allocator before it constructs the Worker, so an exhausted
/// shared memory traps in `frustrate_alloc_aligned`. If that exit tells
/// `PoolWorkers` nothing, no verdict is recorded and the next `callAsync`
/// walks back into a pool with no worker in it — a hang.
///
/// The trap is simulated at the JS frame the runtime already routes every
/// wasm call through, so this needs no test hook in the runtime: the stack
/// reservation is the only call that asks for 1 MiB at 16-byte alignment.
/// The stub throws with the glue's own marker prefix, because that prefix is
/// what tells the Dart side a trap was caught rather than the frame itself
/// having failed (runtime_web.dart, `_call`).
///
/// Its own file because the pool is page-global and initializes once; each
/// `dart test` file gets its own page.
@TestOn('browser')
library;

import 'dart:js_interop';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'guarded_init.dart';

@JS('eval')
external void _eval(String source);

void main() {
  setUpAll(initBridgeGuarded);

  test('a spawn that fails before the worker exists is fatal, not silent', () async {
    if (!Frustrate.instance.asyncIsParallel) {
      markTestSkipped('single-threaded web: async inline, no pool');
      return;
    }
    // Fail every pool-worker stack reservation: `alloc(1 << 20, 16)`.
    _eval(r'''
      (() => {
        const real = globalThis.$frustrateCall;
        globalThis.$frustrateCall = function (f, a, b, c, d, e, g) {
          if (a === 1048576 && b === 16) {
            throw new Error('frustrate-wasm-trap:frustrate test: ' +
                'simulated shared-memory exhaustion');
          }
          return real(f, a, b, c, d, e, g);
        };
      })();
    ''');

    // The first async call creates the pool. The first worker's stack
    // reservation throws out of the hook, which abandons the spawn loop — so
    // no worker was ever constructed, and that first failure is the fatal one.
    await expectLater(
      poolNthPrime(n: 100),
      throwsA(
        isA<BridgePanicException>().having(
          (e) => e.toString(),
          'message',
          contains('simulated shared-memory exhaustion'),
        ),
      ),
    );

    // The decisive one: a later call must reject off the latch. Before the
    // fix nothing latched here, so this call re-entered wasm — and hung (or
    // trapped on a stuck `OnceLock`) instead of failing.
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

    // Scoping, as in pool_init_failure_test: a sync call runs inline on the
    // main instance, and an actor owns its own worker.
    expect(poolWidth(), 4);
    final miner = await Miner.new_(label: 'after the pool died');
    expect(await miner.nthPrime(n: 100), 541);
    await miner.dispose();

    // One report on the way down, and it is the fatal — not a degraded
    // narrowing, since no worker ever came up.
    expect(zoneErrors, isEmpty);
  });
}
