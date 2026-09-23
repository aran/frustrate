/// ActorPool integration: the convenience layer over real actors, on every
/// fixture — native, single-threaded web, threaded web. Unlike the async
/// pool, the actor pool must be genuinely parallel everywhere, including
/// single-threaded web.
///
/// Width probes are Dart-side rendezvous (N concurrent leases all granted
/// at once, or timeout): web actor instances share no memory, so the Rust
/// RENDEZVOUS atomic cannot cross them.
@Timeout(Duration(minutes: 3))
library;

import 'dart:async';
import 'dart:math' show max;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/workers.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Proves [pool] can grant [width] leases simultaneously: each job arrives
/// and holds its lease until all have arrived. A narrower pool deadlocks
/// here instead — hence the timeout.
Future<void> expectFullWidth(ActorPool<Miner> pool, int width) async {
  var arrived = 0;
  final gate = Completer<void>();
  await Future.wait(
    List.generate(
      width,
      (_) => pool.run((m) async {
        arrived++;
        if (arrived == width) gate.complete();
        await gate.future;
      }),
    ),
  ).timeout(const Duration(seconds: 10));
  expect(arrived, width);
}

void main() {
  setUpAll(initBridge);

  // Same ledger guard as bridge_test: a pooled actor's streams must retire with
  // their lease, or the isolate is pinned on a channel whose worker is gone.
  // Labels make a failure name the member rather than just the count.
  tearDown(() async {
    if (!isNativeVm) return;
    for (var i = 0; i < 200 && Frustrate.instance.openChannelCount != 0; i++) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'test leaked an open channel. Still open: '
          '${Frustrate.instance.openChannelLabels}',
    );
  });

  group('sizing', () {
    test('hardwareParallelism is a sane sizing hint', () {
      expect(Frustrate.instance.hardwareParallelism, greaterThanOrEqualTo(1));
    });

    test('defaults to hardwareParallelism; spawn sees slot indices', () async {
      final spawned = <String>[];
      final pool = await ActorPool.spawn((i) async {
        spawned.add('p$i');
        return Miner.new_(label: 'p$i');
      });
      expect(pool.size, Frustrate.instance.hardwareParallelism);
      expect(spawned.toSet(), {for (var i = 0; i < pool.size; i++) 'p$i'});
      await pool.dispose();
    });

    test('explicit size wins; a width below 1 is rejected', () async {
      final pool = await ActorPool.spawn(
        (i) => Miner.new_(label: 'x$i'),
        size: 2,
      );
      expect(pool.size, 2);
      expect(await pool.run((m) => m.label()), startsWith('x'));
      await pool.dispose();
      await expectLater(
        ActorPool.spawn((i) => Miner.new_(label: 'y'), size: 0),
        throwsArgumentError,
      );
    });

    test(
      'a failed spawn disposes what did spawn and strands nothing',
      () async {
        final before = actorHostCount();
        await expectLater(
          ActorPool.spawn(
            (i) =>
                i == 2 ? Miner.flawed(label: 'dud') : Miner.new_(label: 'ok$i'),
            size: 4,
          ),
          throwsA(isA<BridgePanicException>()),
        );
        expect(actorHostCount(), before);
      },
    );
  });

  group('leases', () {
    test(
      'a job holds its instance exclusively for the whole closure',
      () async {
        final pool = await ActorPool.spawn(
          (i) => Miner.new_(label: 'excl$i'),
          size: 2,
        );
        // 6 two-call jobs over 2 instances: each job's counted call must be
        // the only one landing on its instance during the lease.
        await Future.wait(
          List.generate(
            6,
            (_) => pool.run((m) async {
              final beforeJob = await m.calls();
              await m.nthPrime(n: 300);
              expect(
                await m.calls(),
                beforeJob + 1,
                reason: 'another job\'s calls interleaved into this lease',
              );
            }),
          ),
        );
        await pool.dispose();
      },
    );

    test('excess jobs queue FIFO; leases never exceed the width', () async {
      final pool = await ActorPool.spawn(
        (i) => Miner.new_(label: 'q$i'),
        size: 2,
      );
      var inFlight = 0, peak = 0, completed = 0;
      await Future.wait(
        List.generate(
          8,
          (_) => pool.run((m) async {
            inFlight++;
            peak = max(peak, inFlight);
            await m.nthPrime(n: 200);
            inFlight--;
            completed++;
          }),
        ),
      );
      expect(peak, 2);
      expect(completed, 8);
      await pool.dispose();
    });
  });

  group('parallelism', () {
    test(
      'a pool of four runs four Rust bodies at the same time',
      () async {
        rendezvousReset();
        final pool = await ActorPool.spawn(
          (i) => Miner.new_(label: 'pool$i'),
          size: 4,
        );
        // Each job waits inside Rust until all four have arrived, so a pool
        // that ran them one after another would time every one of them out.
        final met = await Future.wait(
          List.generate(
            4,
            (_) => pool.run((m) => m.meet(width: 4, maxWaitMs: 10000)),
          ),
        );
        await pool.dispose();
        expect(met, everyElement(isTrue));
      },
      skip: isNativeVm
          ? false
          : 'web actors are separate wasm instances with no shared counter',
    );
  });

  group('panics', () {
    test('a panic fails exactly its own job; the slot respawns', () async {
      var spawns = 0;
      final pool = await ActorPool.spawn((i) async {
        spawns++;
        return Miner.new_(label: 'slot$i');
      }, size: 2);
      expect(spawns, 2);

      await expectLater(
        pool.run((m) => m.explode()),
        throwsA(isA<BridgePanicException>()),
      );

      await expectFullWidth(pool, 2); // waits out the detached respawn
      expect(spawns, 3, reason: 'the panicked slot respawned exactly once');
      await pool.dispose();
    });

    test('a 5-panic storm never narrows the width', () async {
      var spawns = 0;
      final pool = await ActorPool.spawn((i) async {
        spawns++;
        return Miner.new_(label: 'storm$i');
      }, size: 2);
      for (var i = 0; i < 5; i++) {
        await expectLater(
          pool.run((m) => m.explode()),
          throwsA(isA<BridgePanicException>()),
        );
        // The pool keeps serving between explosions.
        expect(await pool.run((m) => m.nthPrime(n: 50)), isPositive);
      }
      await expectFullWidth(pool, 2);
      expect(spawns, 7, reason: 'five panics, five respawns, width intact');
      await pool.dispose();
    });

    test('a plain error does not retire the instance', () async {
      var spawns = 0;
      final pool = await ActorPool.spawn((i) async {
        spawns++;
        return Miner.new_(label: 'err$i');
      }, size: 1);
      await expectLater(
        pool.run((m) => m.checkedDiv(a: 1, b: 0)),
        throwsA(isA<BridgeException>()),
      );
      expect(
        await pool.run((m) => m.calls()),
        0,
        reason: 'same instance, state intact — not respawned',
      );
      expect(spawns, 1);
      await pool.dispose();
    });

    test(
      'a failed respawn breaks the pool loudly, never silently narrower',
      () async {
        var spawns = 0;
        final pool = await ActorPool.spawn((i) async {
          spawns++;
          if (spawns > 2) throw StateError('factory out of parts');
          return Miner.new_(label: 'poison$i');
        }, size: 2);
        await expectLater(
          pool.run((m) => m.explode()),
          throwsA(isA<BridgePanicException>()),
        );

        // The respawn fails detached from any job; poll until the pool
        // reports itself broken (healthy-slot jobs may still succeed first).
        StateError? failure;
        for (var i = 0; i < 200 && failure == null; i++) {
          try {
            await pool.run((m) => m.label());
          } on StateError catch (e) {
            failure = e;
          }
          await Future<void>.delayed(const Duration(milliseconds: 5));
        }
        expect(
          failure,
          isNotNull,
          reason: 'a broken pool must refuse jobs, not shrink quietly',
        );
        expect('$failure', contains('out of parts'));
        await pool.dispose();
      },
    );
  });

  group('streams', () {
    // A stream member takes its sink as an ordinary parameter, so feeding
    // ActorPool.stream (which is about leases, not about codegen) means
    // binding a controller and handing back its stream.
    Stream<int> mine(Miner m, int rounds, int n) {
      final c = StreamController<int>();
      unawaited(m.mineProgress(rounds: rounds, n: n, sink: c));
      return c.stream;
    }

    test('a streaming job holds its lease until done', () async {
      final pool = await ActorPool.spawn(
        (i) => Miner.new_(label: 's$i'),
        size: 1,
      );
      final items = await pool.stream((m) => mine(m, 3, 50)).toList();
      expect(items, hasLength(3));
      // The lease came back: a plain run on the width-1 pool is grantable
      // and sees the mining rounds on the same instance.
      expect(await pool.run((m) => m.calls()), 3);
      await pool.dispose();
    });

    test('cancelling the stream releases the lease', () async {
      final pool = await ActorPool.spawn(
        (i) => Miner.new_(label: 'c$i'),
        size: 1,
      );
      final first = await pool.stream((m) => mine(m, 50, 30)).first;
      expect(first, isPositive);
      // .first cancelled the stream; the width-1 pool must grant again
      // (FIFO behind whatever mining the cancel has yet to interrupt).
      expect(await pool.run((m) => m.label()), 'c0');
      await pool.dispose();
    });
  });

  group('dispose', () {
    test('drains accepted jobs, rejects new ones, idempotent', () async {
      final pool = await ActorPool.spawn(
        (i) => Miner.new_(label: 'd$i'),
        size: 1,
      );
      final running = pool.run((m) => m.nthPrime(n: 2000));
      final queued = pool.run((m) => m.calls());
      final disposing = pool.dispose();

      await expectLater(pool.run((m) => m.label()), throwsStateError);
      expect(
        await running,
        isPositive,
        reason: 'the running job completes through the drain',
      );
      expect(
        await queued,
        1,
        reason: 'the queued job was accepted before dispose and completes',
      );
      await disposing;
      await pool.dispose(); // idempotent
      // Rejected while acquiring the lease, so the job closure never runs.
      await expectLater(
        pool.stream((m) => StreamController<int>().stream).toList(),
        throwsStateError,
      );
    });
  });
}
