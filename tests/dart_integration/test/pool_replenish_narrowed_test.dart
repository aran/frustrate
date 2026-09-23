/// Replenishment restores a width that was never whole.
///
/// `pool_degraded_test` pins one order — a worker traps, and the replenishment
/// that trap triggers is *itself* the spawn that fails. The reverse had no
/// coverage: a pool **already narrowed** by a failed spawn, which then loses a
/// *live* worker to a trap, so replenishment is asked to restore a width that
/// was never whole. The two paths meet in `PoolWorkers` (`spawnThrew` then
/// `retire`), and nothing exercised them in that order.
///
/// One injection does the whole thing, which is the trick. The `if (++n === 3)
/// throw` patch below is one-shot by construction: constructions 1 and 2
/// succeed, 3 throws and unwinds out of the pool's spawn loop (abandoning 4 as
/// well), leaving a 2-wide pool — and construction **4, the replenishment
/// after the trap, succeeds**. Neither other lever can produce that shape: a
/// 404 `$frustrateGlueUrl` breaks every later spawn (that is
/// `pool_degraded_test`), and the `$frustrateCall` stub is keyed on argument
/// shape rather than on call count.
///
/// The counter is why this file spawns no actor: patching `@JS('Worker')`
/// intercepts actor worker creation too, and one extra construction anywhere on
/// the page shifts which spawn is refused and which replenishment succeeds. For
/// the same reason, a future change that creates a Worker during init (glue
/// preloading, a diagnostics worker) will shift `n` and must move it here.
///
/// Its own file because the patch, like the spawn latch it trips, is per page,
/// and `package:test` runs a file's tests sequentially on one page — a second
/// test here would run against pool state an earlier failure left unknown and
/// report cascade noise under a misleading name.
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

  test('a trap on an already-narrowed pool replenishes back to its real width', () async {
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

    // 1. Narrow the pool. The first async call creates it: workers 1 and 2 come
    //    up, worker 3 throws out of the hook, and the exception unwinds into
    //    this call.
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

    // The degraded report is deferred a microtask and is unordered with respect
    // to the rejection above, so it has to be waited for rather than assumed.
    await until(() => zoneErrors.isNotEmpty, 'the degraded report');
    expect(
      (zoneErrors.single as StateError).message,
      allOf(
        contains('could not be created'),
        contains('running at 2 worker(s)'),
      ),
    );

    // 2. The real width, measured rather than inferred from `_live` arithmetic.
    //    The probe returns true only if that many probes were resident on
    //    distinct workers at the same instant. RENDEZVOUS is page-global, so
    //    every arm resets it, and the `Future.wait` is what guarantees the reset
    //    lands while the counter is quiescent.
    rendezvousReset();
    expect(
      await Future.wait(
        List.generate(2, (_) => poolRendezvous(width: 2, maxWaitMs: 10000)),
      ),
      everyElement(isTrue),
      reason: 'the narrowed pool starts out genuinely 2 wide',
    );

    // 3. Trap one of the two survivors. This is a worker that *ran*, so the
    //    runtime retires it and replenishes — construction #4, which the
    //    one-shot patch lets through. Momentarily the pool is 1 wide.
    await expectLater(poolPanic(), throwsA(isA<BridgePanicException>()));

    // Serving calls again, and returning the right answer through the
    // trap-and-replenish path rather than merely being schedulable. Claims no
    // coverage of its own — step 4 subsumes it — but it makes a total failure
    // read as "cannot serve at all" instead of "cannot muster width 2".
    expect(
      await poolNthPrime(n: 100),
      541,
      reason: 'the replenished pool must serve, and serve correctly',
    );

    // 4. The assertion that earns this file: the width the failed spawn left is
    //    restored, by a replenishment that had to rebuild a pool it never saw
    //    whole. The 10 s budget also absorbs the replacement worker's spin-up —
    //    it fetches a cached script and instantiates an already-compiled Module
    //    handed over by structured clone, but that is still hundreds of ms.
    rendezvousReset();
    expect(
      await Future.wait(
        List.generate(2, (_) => poolRendezvous(width: 2, maxWaitMs: 10000)),
      ),
      everyElement(isTrue),
      reason:
          'replenishment must restore the width the pool actually had, '
          'not the width Rust thinks it was configured for',
    );

    // 5. And not overshoot. `contains(false)`, never `everyElement(isFalse)`:
    //    the third probe is queued behind the two spinners and arrives to find
    //    the counter already at 2. Last, because it costs a hard wall-clock
    //    `max_wait_ms` — 2000 ms, matching pool_degraded_test, priced for the
    //    failing run: a build that retried the dead spawn would produce its
    //    third worker a real browser spin-up late, and a shorter budget would
    //    time out before it arrived and report that regression green.
    rendezvousReset();
    expect(
      await Future.wait(
        List.generate(3, (_) => poolRendezvous(width: 3, maxWaitMs: 2000)),
      ),
      contains(false),
      reason:
          'replenishment replaces the worker that trapped — it must not '
          'also resurrect the spawn that failed',
    );

    // Still one report, over a history no other file covers: spawn failure,
    // then trap, then a *successful* replenish. Deliberately narrow about what
    // this proves — the `_degradedReported` latch (pool_workers.dart:54) was
    // already set in step 1, so this cannot distinguish `retire` from
    // `initFailed` on the trap path: `initFailed` here would leave one live
    // worker, take the suppressed degraded branch, and the replenish (a
    // separate statement, runtime_web.dart:505) would restore the width anyway.
    // That substitution is caught on an *unlatched* pool, by bridge_test's 'a
    // panic never narrows the pool', where the spurious report reaches the
    // suite's own zone and fails the test outright. Nor does this line notice a
    // replenishment that fails outright: measured, by refusing worker #4 as
    // well — every later `spawnThrew` is suppressed by the same latch, this
    // line still passes, and step 4 is what goes red. What is left for it is
    // the latch itself across a richer event history than any other file
    // produces — a latch made per-cause, or reset on a successful replenish,
    // shows up here and nowhere else.
    expect(
      zoneErrors,
      hasLength(1),
      reason:
          'one loss, one report — the trap that followed it was '
          'replaceable, so it is not a second narrowing',
    );
  });
}
