/// A pool spawn that fails *part way through* leaves a working pool, not a
/// wedged one.
///
/// The hazard: a `spawn_worker` hook that throws unwinds out of wasm with
/// nothing running under panic=abort. If pool construction sits behind a
/// `OnceLock`, that leaves the `Once` in RUNNING for the life of the page, and
/// the next `pool()` reaches `futex_wait` — `memory.atomic.wait32`, which
/// traps on the browser main thread. Every later async call on the page then
/// dies, with live pool workers sitting idle and unreachable.
///
/// So the assertion has to be that a *later* call still runs, on the workers
/// that did come up. Checking only that the triggering call rejects would
/// pass against the wedged build.
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

/// Make the 3rd `new Worker(...)` on this page throw, and let every other
/// construction through. Stands in for any spawn that fails after some
/// workers are already up — a CSP page, an exhausted shared memory (the
/// stack/TLS allocation traps), a browser worker limit.
@JS('eval')
external void _eval(String source);

void main() {
  setUpAll(initBridgeGuarded);

  test('a spawn that fails mid-sequence leaves the pool usable', () async {
    if (!Frustrate.instance.asyncIsParallel) {
      markTestSkipped('single-threaded web: async inline, no pool');
      return;
    }
    _eval(r'''
      (() => {
        const Real = globalThis.Worker;
        let n = 0;
        globalThis.Worker = function (url, opts) {
          if (++n === 3) throw new Error('frustrate test: worker #3 refused');
          return new Real(url, opts);
        };
      })();
    ''');

    // The first async call creates the pool: workers 1 and 2 come up, worker 3
    // throws out of the hook, and the exception unwinds into this call.
    await expectLater(
      poolNthPrime(n: 100),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('worker #3'),
        ),
      ),
    );

    // The decisive one: two workers are alive, so a later call must actually
    // run — not trap on a stuck `Once`, and not queue behind nothing.
    expect(await poolNthPrime(n: 100), 541);
    expect(await poolNthPrime(n: 1000), 7919);

    // And the loss was reported once, out of band, as a narrowing rather than
    // a fatal: there is no call it would be honest to fail.
    await until(() => zoneErrors.isNotEmpty, 'the degraded report');
    expect(zoneErrors, hasLength(1));
    expect(
      (zoneErrors.single as StateError).message,
      allOf(
        contains('could not be created'),
        contains('running at 2 worker(s)'),
      ),
    );

    // Everything above is still compatible with a pool that runs one call at a
    // time: 'running at 2 worker(s)' is `PoolWorkers`' own arithmetic over
    // `_live`, which counts *candidates*, and a single `poolNthPrime` is
    // satisfied by a single worker. The two arms below measure the width the
    // failed spawn actually left, using the rendezvous probe: it returns true
    // only if that many probes were resident on distinct workers at the same
    // instant, so it is a width measurement and not a timing heuristic.
    //
    // The 2 is constructions 1 and 2 — the ones the patch above let through.
    // RENDEZVOUS is page-global, so each arm resets it, and the `Future.wait`
    // is what guarantees the reset lands while the counter is quiescent (every
    // probe increments before it can return, so an awaited batch has finished
    // touching the counter).
    rendezvousReset();
    expect(
      await Future.wait(
        List.generate(2, (_) => poolRendezvous(width: 2, maxWaitMs: 10000)),
      ),
      everyElement(isTrue),
      reason:
          'the two workers that did come up must run two calls at once, '
          'not take turns',
    );

    // The other side of the bracket, and last because it costs a hard
    // wall-clock `max_wait_ms`. `contains(false)`, never `everyElement(isFalse)`
    // — the third probe is queued behind the two spinners and arrives to find
    // the counter already at 2, so it returns true.
    //
    // On the budget: at true width 2 the outcome is budget-independent (a third
    // arrival is needed for >= 3, and it cannot be dispatched until a worker
    // frees, so whichever spinner times out first is a guaranteed false). The
    // budget is priced for the FAILING run instead — it has to be long enough
    // that a pool which really did muster three workers rendezvouses inside it,
    // or the arm silently loses its power to go red. The regression it exists
    // for is a reintroduced spawn retry, whose third worker arrives a real
    // browser worker spin-up late (fetch + instantiate + loop entry, hundreds
    // of ms on loaded CI), so a short budget would time out first and report
    // green. Hence the same 2000 ms as pool_degraded_test, for the same reason.
    rendezvousReset();
    expect(
      await Future.wait(
        List.generate(3, (_) => poolRendezvous(width: 3, maxWaitMs: 2000)),
      ),
      contains(false),
      reason:
          'the abandoned spawns must not have come up behind our back — '
          'a failed spawn is never retried (a no-progress loop)',
    );
  });
}
