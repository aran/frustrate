/// Workers outlive the Dart heap that created them, so a fresh heap has to
/// reap the previous one's.
///
/// A Flutter web hot restart resets the Dart heap and re-runs `main()` without
/// unloading the document. The old `WebRuntime`, its `PoolWorkers` set, and
/// every Dart handle to its Workers all vanish; the Workers do not — each pool
/// worker is still parked in `frustrate_worker_entry` on a shared memory
/// nothing can address any more. That is why a `dispose()` is not the fix: no
/// Dart teardown of any kind runs on hot restart, so there is nobody left to
/// call one. The only place a fresh heap can still find those Workers is the
/// page itself, which is why the runtime registers them on `globalThis` and
/// reaps whatever it finds there on the first init of a heap.
///
/// This suite cannot reset a Dart heap — `package:test` gives a file one page
/// and one heap. What it can do is manufacture the JS-observable state a hot
/// restart leaves behind (live Workers, registered on `globalThis`, with no
/// Dart owner) *before* the page's first init, and require the init to kill
/// them. `Frustrate.isInstalled` is genuinely false at that moment, exactly as
/// it is in a restarted heap; nothing is faked but how the orphans got there.
///
/// The second test is what ties that to the real thing: it shows the runtime
/// putting its own pool workers into the same registry, proves those entries
/// are Workers the page really constructed, and proves with the rendezvous
/// probe that they are genuinely resident rather than merely recorded. The one
/// step neither test can execute is the heap reset itself, which is safe by
/// construction: `globalThis` is already load-bearing across that boundary —
/// `_installGlue` returns early when `$frustrateCall` survives from a previous
/// life, and `$frustrateGlueUrl` is read the same way.
///
/// The orphans beat on a timer rather than running the glue, because liveness
/// has to be *measured*. A real pool worker parked in `memory.atomic.wait32`
/// never returns to its event loop, so it cannot answer anything — it is
/// indistinguishable from a terminated worker by message. A heartbeat is the
/// only instrument that separates "alive" from "gone"; what is under test is
/// termination, and `terminate()` does not care what the worker was running.
///
/// Runs on both web fixtures: the reap is a property of init, not of threads.
/// Its own file because it seeds page-global state before init.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:js_interop_unsafe';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'guarded_init.dart';

@JS('eval')
external void _eval(String source);

/// The page registry the runtime is required to keep its Workers in.
const String _registryKey = r'$frustrateWorkers';

/// How many orphans a "previous heap" left behind. Not the pool width: these
/// stand in for any Worker frustrate created and lost, and using a different
/// number keeps the two tests' arithmetic from coinciding by accident.
const int _orphanCount = 3;

void _seedPreviousHeap() {
  _eval(
    r'''
    (() => {
      globalThis.$frustrateTestBeats = 0;
      const src = 'setInterval(() => postMessage(1), 5);';
      const url = URL.createObjectURL(
          new Blob([src], { type: 'text/javascript' }));
      globalThis.$frustrateWorkers = globalThis.$frustrateWorkers || [];
      for (let i = 0; i < ''' +
        '$_orphanCount' +
        r'''; i++) {
        const w = new Worker(url);
        w.onmessage = () => { globalThis.$frustrateTestBeats++; };
        globalThis.$frustrateWorkers.push(w);
      }
    })();
  ''',
  );
}

/// Record every Worker constructed from here on. Installed after the seeding
/// and before init, so the capture list is exactly the runtime's own workers.
void _captureWorkers() {
  _eval(r'''
    (() => {
      const Real = globalThis.Worker;
      globalThis.$frustrateTestWorkers = [];
      globalThis.Worker = function (url, opts) {
        const w = new Real(url, opts);
        globalThis.$frustrateTestWorkers.push(w);
        return w;
      };
      // `every` alone is vacuously true on an empty registry, which is the
      // pre-fix state — so require a non-empty one explicitly rather than
      // leaning on the length assertion next to the call site.
      globalThis.$frustrateTestRegisteredAreReal = () =>
          (globalThis.$frustrateWorkers || []).length > 0 &&
          globalThis.$frustrateWorkers.every(
              (w) => globalThis.$frustrateTestWorkers.includes(w));
    })();
  ''');
}

@JS(r'$frustrateTestRegisteredAreReal')
external bool _registeredAreReal();

/// Messages received from the seeded orphans. Frozen once they are gone.
int get _beats =>
    globalContext.getProperty<JSNumber>(r'$frustrateTestBeats'.toJS).toDartInt;

/// The registry as the runtime left it. Absent means "the runtime keeps no
/// such thing", which is the pre-fix state and must read as empty rather than
/// throw, so the failure lands on the assertion rather than on a cast.
List<JSObject> _registry() {
  final v = globalContext.getProperty<JSAny?>(_registryKey.toJS);
  if (v == null || !v.isA<JSArray<JSAny?>>()) return const [];
  return (v as JSArray<JSObject>).toDart;
}

void main() {
  setUpAll(() async {
    _seedPreviousHeap();
    // The orphans must be demonstrably running *before* init, or the silence
    // afterwards proves nothing. 20 beats at a 5 ms interval is a tenth of a
    // second of evidence, and `until` bounds the wait.
    await until(() => _beats >= 20, 'the seeded orphans to start beating');
    _captureWorkers();
    await initBridgeGuarded();
  });

  test(
    'the first init of a heap reaps the Workers the previous one left',
    () async {
      // Let anything already queued land before taking the reading: terminate()
      // stops the worker, not the messages the task queue is already holding.
      await Future<void>.delayed(const Duration(milliseconds: 200));
      final settled = _beats;
      await Future<void>.delayed(const Duration(milliseconds: 500));

      expect(
        _beats,
        settled,
        reason:
            'each orphan beats every 5 ms and there are $_orphanCount of '
            'them, so half a second of complete silence is them being gone — '
            'a hot restart that left them running would show ~300 beats here',
      );
      expect(
        _registry(),
        isEmpty,
        reason:
            'a reaped worker must leave the registry too, or the next '
            'heap reaps a corpse and the list grows without bound',
      );
    },
  );

  test('the pool registers its workers where the next heap will find them', () async {
    if (!Frustrate.instance.asyncIsParallel) {
      markTestSkipped('single-threaded web: async inline, no pool');
      return;
    }
    final width = poolWidth();
    expect(width, greaterThan(1));

    // The first async call is what creates the pool.
    expect(await poolNthPrime(n: 100), 541);

    expect(
      _registry(),
      hasLength(width),
      reason:
          'a heap that restarts now must be able to find every pool '
          'worker this one started; anything it cannot find runs forever',
    );
    expect(
      _registeredAreReal(),
      isTrue,
      reason:
          'every registry entry must be a Worker this page actually '
          'constructed, not a placeholder',
    );

    // And they are resident, not merely recorded: the probe returns true only
    // if that many calls were running on distinct workers at the same instant.
    rendezvousReset();
    expect(
      await Future.wait(
        List.generate(
          width,
          (_) => poolRendezvous(width: width, maxWaitMs: 10000),
        ),
      ),
      everyElement(isTrue),
      reason:
          'the registered workers are the pool, so a registry that '
          'matched the width while the pool ran narrower would be lying',
    );
  });
}
