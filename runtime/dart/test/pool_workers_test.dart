/// The threaded-wasm pool's spawn-failure contract, which the web transport
/// reaches through [PoolWorkers]:
///
/// - a worker that fails to start is never retried — but the pool keeps
///   running while anything is left to drain the queue, because failing the
///   calls in flight would reject work that was about to succeed;
/// - fatal means the *last* worker is gone, and it latches, so the issue path
///   can reject new calls instead of hanging them;
/// - each outcome is reported once per pool, not once per worker.
///
/// Tested here rather than end-to-end because the seam is pure Dart — the
/// browser can only produce all-or-nothing spawn failures, so the sequencing
/// below (partial loss, then the last one) has no other precise pin. Same
/// reason `pending_calls_test` and `stream_router_test` exist.
@TestOn('vm')
library;

import 'package:frustrate/src/pool_workers.dart';
import 'package:test/test.dart';

/// Stands in for `_JSWorker`: the seam only ever identifies and terminates
/// one, so a name is enough.
final class FakeWorker {
  final String name;
  FakeWorker(this.name);
  @override
  String toString() => name;
}

/// A pool wired to record what it reported, in order.
({
  PoolWorkers<FakeWorker> pool,
  List<FakeWorker> terminated,
  List<StateError> fatal,
  List<StateError> degraded,
})
makePool() {
  final terminated = <FakeWorker>[];
  final fatal = <StateError>[];
  final degraded = <StateError>[];
  return (
    pool: PoolWorkers<FakeWorker>(
      terminate: terminated.add,
      onFatal: fatal.add,
      onDegraded: degraded.add,
    ),
    terminated: terminated,
    fatal: fatal,
    degraded: degraded,
  );
}

List<FakeWorker> spawn(PoolWorkers<FakeWorker> pool, int n) {
  final workers = [for (var i = 0; i < n; i++) FakeWorker('w$i')];
  workers.forEach(pool.add);
  return workers;
}

void main() {
  test(
    'losing one worker of four degrades: no latch, the pool keeps running',
    () {
      final p = makePool();
      final workers = spawn(p.pool, 4);

      p.pool.initFailed(workers[0], 'script 404');

      expect(p.pool.live, 3);
      expect(
        p.pool.failure,
        isNull,
        reason:
            'three workers can still drain the queue, so the calls in '
            'flight must be left alone',
      );
      expect(p.terminated, [workers[0]]);
      expect(p.fatal, isEmpty);
      expect(p.degraded, hasLength(1));
      expect(p.degraded.single.message, contains('script 404'));
      expect(p.degraded.single.message, contains('3 worker(s)'));
    },
  );

  test('losing the last worker is fatal, and the error carries its cause', () {
    final p = makePool();
    final workers = spawn(p.pool, 2);

    p.pool.initFailed(workers[0], 'script 404');
    expect(p.pool.failure, isNull);

    p.pool.initFailed(workers[1], 'instantiate failed: bad import');

    expect(p.pool.live, 0);
    expect(p.pool.failure, isNotNull);
    expect(p.fatal, hasLength(1));
    expect(p.fatal.single, same(p.pool.failure));
    expect(p.fatal.single.message, contains('instantiate failed: bad import'));
    expect(
      p.fatal.single.message,
      contains('async'),
      reason:
          'the message must scope the damage: sync calls and actor '
          'hosts still work',
    );
    expect(p.terminated, workers);
  });

  test(
    'a pool that fails wholesale reports once degraded, then once fatal',
    () {
      final p = makePool();
      final workers = spawn(p.pool, 4);

      for (final worker in workers) {
        p.pool.initFailed(worker, 'script 404');
      }

      // Not four reports. The first loss is a true statement about a pool that
      // was still running; the rest are the same fact restated, and the fatal
      // supersedes them.
      expect(p.degraded, hasLength(1));
      expect(p.fatal, hasLength(1));
      expect(p.terminated, workers);
    },
  );

  test('reporting a worker twice changes nothing', () {
    final p = makePool();
    final workers = spawn(p.pool, 2);

    p.pool.initFailed(workers[0], 'script 404');
    p.pool.initFailed(workers[0], 'script 404');

    expect(p.pool.live, 1, reason: 'the second report must not double-count');
    expect(p.terminated, [workers[0]]);
    expect(p.degraded, hasLength(1));
    expect(p.fatal, isEmpty);
  });

  test('retire decides nothing, even when it empties the pool', () {
    final p = makePool();
    final workers = spawn(p.pool, 1);

    // The trap path: the caller replenishes immediately afterwards, so an
    // empty set here is a moment, not a verdict.
    p.pool.retire(workers.single);

    expect(p.pool.live, 0);
    expect(p.pool.failure, isNull);
    expect(p.fatal, isEmpty);
    expect(p.degraded, isEmpty);
    expect(p.terminated, workers);

    // And a replacement that fails to start then *is* fatal.
    final replacement = FakeWorker('replacement');
    p.pool.add(replacement);
    p.pool.initFailed(replacement, 'script 404');
    expect(p.pool.failure, isNotNull);
  });

  test('retiring an unknown worker is a no-op', () {
    final p = makePool();
    spawn(p.pool, 1);

    p.pool.retire(FakeWorker('never added'));

    expect(p.pool.live, 1);
    expect(p.terminated, isEmpty);
  });

  test(
    'a worker that could not be constructed is fatal when it was the pool',
    () {
      final p = makePool();

      // CSP blocked `new Worker(blob:…)`: nothing was ever added, so there is
      // nothing to drain the queue.
      p.pool.spawnThrew('SecurityError: blob: workers blocked');

      expect(p.pool.failure, isNotNull);
      expect(p.fatal.single.message, contains('SecurityError'));
      expect(p.degraded, isEmpty);
    },
  );

  test('the fatal latch is permanent and reports only once', () {
    final p = makePool();
    final workers = spawn(p.pool, 1);

    p.pool.initFailed(workers.single, 'first');
    final latched = p.pool.failure;

    p.pool.spawnThrew('second');
    p.pool.add(FakeWorker('late'));

    expect(
      p.pool.failure,
      same(latched),
      reason: 'the first cause is the one that explains the pool',
    );
    expect(p.fatal, hasLength(1));
  });
}
